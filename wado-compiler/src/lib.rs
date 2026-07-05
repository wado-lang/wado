pub mod analyze;
pub mod ast;
pub mod ast_index;
pub mod bind;
pub mod builtin_registry;
pub mod cm_abi;
pub mod codegen;
pub mod codegen_flags;
pub mod comment;
pub mod compiler_host;
pub mod compiler_item;
pub mod component_model;
pub(crate) mod const_eval;
pub mod doc;
pub mod effect_check;
pub mod elaborator;
pub mod flat_package;
pub mod hashmap;
pub mod intern;
pub mod kiln;
pub mod lexer;
pub mod link;
pub mod loader;
pub mod logger;
pub mod lower;
pub mod module_source;
pub mod monomorphize;
pub mod name;
pub mod nir;
pub mod nir_arena;
pub mod nir_engine;
pub mod nir_package;
pub mod nir_unparse;
pub mod nir_value_graph;
pub mod nir_visitor;
pub mod optimize;
pub mod package;
pub mod param_resolution;
pub mod parser;
pub mod path;
pub mod remarks;
pub mod semantics;
pub mod stdlib;
pub(crate) mod stdlib_snapshot;
pub mod test_names;
pub use stdlib_snapshot::prewarm as prewarm_stdlib_snapshot;
pub mod niri;
pub mod symbol;
pub mod symbol_notation;
pub mod syntax;
pub mod synthesis;
pub mod tir;
pub mod tir_visitor;
pub mod token;
pub mod trace;
pub mod unparse;
pub mod wir;
pub mod wir_build;
pub mod wir_optimize;
pub mod wir_unparse;
pub mod wir_visitor;
pub mod wit_bundle;
pub mod wit_consume;
pub mod wit_emit;
pub mod world_registry;

pub use analyze::Analyzer;
pub use ast::{AstId, AstNodeKind, AstPtr};
pub use bind::{BindError, Binder};
pub use codegen_flags::CodegenFlags;
pub use compiler_host::{
    Code, CompilerHost, DependencyIndex, Diagnostic, DiagnosticSpan, GeneratorDiagnostic,
    GeneratorDiagnosticLevel, GeneratorError, GeneratorInputFile, GeneratorOutputFile,
    GeneratorReadRecord, GeneratorRequest, GeneratorResponse, GeneratorRunnerError,
    GeneratorSourceSpan, KILN_GENERATOR_WIT, LogLevel, Severity, SourceError,
};
pub use logger::{Bail, Logger};
pub use remarks::{Remark, collect_value_copy_remarks};
pub use semantics::{
    Cursor, Definition, Semantics, SymbolResolveError, lex_error_diagnostic,
    parse_error_diagnostic, semantics, semantics_for_world, semantics_of,
};

#[cfg(test)]
pub use compiler_host::InMemoryCompilerHost;
pub use effect_check::{
    DefaultPurityError, EffectError, SemanticDiagnostics, StoresError,
    check_default_purity_semantic, check_effects_semantic, check_semantics, check_stores_semantic,
};
pub use elaborator::{Elaborator, TypeError};
pub use flat_package::FlatPackage;
pub use lexer::{LexError, LexErrorKind, LexResult, lex, lex_with_line};
pub use loader::{LoadError, LoadResult, ModuleLoader};
pub use lower::lower;
pub use module_source::ModuleSource;
pub use monomorphize::monomorphize;
pub use optimize::{OptLevel, optimize};
pub use package::Package;
pub use parser::{ParseError, Parser};
pub use token::Span;

/// Build the diagnostic message for an unresolved `Type^Trait::method` call —
/// `Type` does not implement `Trait` (see the WIR-build trait-bound check).
fn trait_bound_violation_message(call_name: &str) -> String {
    if let Some((ty, trait_name)) = name::split_trait_method_receiver(call_name) {
        format!("type `{ty}` does not implement trait `{trait_name}`")
    } else {
        format!("unresolved generic call `{call_name}`")
    }
}

use std::cell::RefCell;
use std::rc::Rc;

use crate::hashmap::IndexMap;

/// Result of compiling a Wado source file
#[derive(Debug)]
pub struct CompileResult {
    /// Compiled WebAssembly component bytes
    pub wasm: Vec<u8>,
    /// Parsed module AST (includes data section if present)
    pub module: ast::Module,
    /// WIR module (retained when `CompilerOptions::retain_wir` is true)
    pub wir_package: Option<wir::WirPackage>,
    /// Whether the entry module has `#![TODO]`
    pub is_todo_module: bool,
    /// Structural description of the generator's `pub struct Options`,
    /// populated only when `target_world == "core:kiln/generator"` and
    /// the Options struct extracts cleanly. Consumed by the CLI's kiln
    /// provider to skip a second compile when the driver asks for the
    /// descriptor.
    pub kiln_options_descriptor: Option<kiln::OptionsDescriptor>,
}

/// Compilation failure with metadata from the successfully-parsed AST.
///
/// Internal `Bail` carries no data (errors are already emitted to the host).
/// This wrapper adds the `is_todo_module` flag so callers can distinguish
/// expected failures in `#![TODO]` modules from real errors.
#[derive(Debug)]
pub struct CompileFailure {
    /// Whether the entry module has `#![TODO]`
    pub is_todo_module: bool,
}

impl std::fmt::Display for CompileFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compilation failed")
    }
}

impl std::error::Error for CompileFailure {}

/// Result of dumping compiler internal state
#[derive(Debug)]
pub struct DumpResult {
    /// Source code
    pub source: String,
    /// Tokens from lexer
    pub tokens: Vec<token::Token>,
    /// The main module's AST (after parser).
    pub ast: ast::Module,
    /// Symbol table after analysis
    pub symbols: symbol::SymbolTable,
    /// Loaded module sources
    pub loaded_modules: Vec<ModuleSource>,
    /// Modules loaded implicitly by the compiler
    pub implicit_modules: Vec<ModuleSource>,
    /// Entry module source
    pub entry_module_source: ModuleSource,
    /// All TIR modules after resolution (in topological order)
    pub tir_modules: Option<IndexMap<ModuleSource, tir::TirModule>>,
    /// Monomorphized TIR snapshot (unparsed text)
    pub monomorphized_tir_text: Option<String>,
    /// Lowered TIR snapshot (unparsed text)
    pub lowered_nir_text: Option<String>,
    /// Optimized NIR package — the post-`optimize` body IR consumed by
    /// `wir_build` and `codegen`. See WEP `wep-2026-05-11-nir.md`.
    pub optimized_package: Option<nir_package::NirPackage>,
    /// WIR module (after `tir_to_wir` translation)
    pub wir_package: Option<wir::WirPackage>,
    /// AstId-keyed trivia for unparsing the dumped AST.
    pub trivia: comment::TriviaMap,
}

/// Compilation options for the compiler
#[derive(Debug, Clone)]
pub struct CompilerOptions {
    /// Optimization level
    pub opt_level: OptLevel,
    /// Target world fully-qualified name (e.g., "wasi:cli/command", "wasi:http/service")
    pub target_world: Option<String>,
    /// Skip Wasm validation after code generation.
    /// When true, the compiler returns raw Wasm bytes even if they fail validation.
    /// Useful for debugging the code generator.
    pub skip_validation: bool,
    /// When true, retain the WIR module in [`CompileResult::wir_package`].
    /// Used by test infrastructure to inspect WIR without a second compilation pass.
    pub retain_wir: bool,
    /// Override the inline threshold for the optimization pass.
    /// When `None`, the default for the `opt_level` is used.
    pub inline_threshold: Option<usize>,
    /// Override the number of fixed-point optimization iterations.
    /// When `None`, the default for the `opt_level` is used.
    pub opt_iterations: Option<u32>,
    /// Log level for compiler diagnostics.
    /// When `None`, uses the default (`Info`).
    pub log_level: Option<LogLevel>,
    /// Which allocator to use (e.g., `"bump"`, `"debug"`).
    /// Matches `#[allocator("...")]` attributes in `core:allocator`.
    /// When `None`, the compiler auto-selects: `"debug"` for test world, `"bump"` otherwise.
    pub allocator: Option<String>,
    /// Kiln invocation redirects consulted by the module loader. Callers
    /// that have run the Kiln pipeline (wado-cli, wado-lsp) populate this;
    /// everyone else leaves it empty.
    pub invocations: kiln::InvocationIndex,
    /// `--test-name` substring filters for the test world. When non-empty,
    /// only `test "name"` blocks whose name contains one of these strings are
    /// exported; the rest become dead code and are removed by early DCE, so
    /// filtered-out tests are never compiled into the output. Empty means
    /// "run every test". Ignored outside the test world.
    pub test_name_filters: Vec<String>,
    /// Raw codegen feature flags forwarded from the CLI's generic `-f <flag>`
    /// option (e.g. `["array-copy"]`). Parsed into [`CodegenFlags`] during
    /// compilation; an unrecognized flag is a hard error. Empty by default.
    pub codegen_flags: Vec<String>,
    /// Emit unused diagnostics (`DeadFunction` / `DeadGlobal`, …). On by
    /// default; the CLI's `--no-unused` flag turns it off. Gates only the
    /// diagnostic emission, never the liveness analysis itself.
    pub unused_diagnostics: bool,
    /// Library world FQ (`namespace:name/name@version`) for `wado compile
    /// --lib`. When `Some`, the compiler synthesizes a library world from the
    /// entry module's `export fn`s — one Component Model export per function —
    /// instead of conforming to a fixed WASI world. Sets `target_world` to this
    /// FQ and bypasses the static world-registry lookup.
    pub lib_world: Option<String>,
    /// Compile-time parameter overrides from the CLI's `-D NAME=value` flags.
    /// Consumed by the param-resolution pass against `#[param]` globals; an
    /// entry matching no declaration is reported per `param_policy.unknown`.
    pub param_overrides: crate::hashmap::IndexMap<String, String>,
    /// Severity policy for the three param-resolution diagnostic classes
    /// (`--param-unknown` / `--param-invalid` / `--param-missing`).
    pub param_policy: param_resolution::ParamPolicy,
}

impl Default for CompilerOptions {
    fn default() -> Self {
        Self {
            opt_level: OptLevel::default(),
            target_world: None,
            skip_validation: false,
            retain_wir: false,
            inline_threshold: None,
            opt_iterations: None,
            log_level: None,
            allocator: None,
            invocations: kiln::InvocationIndex::default(),
            test_name_filters: Vec::new(),
            codegen_flags: Vec::new(),
            unused_diagnostics: true,
            lib_world: None,
            param_overrides: crate::hashmap::IndexMap::default(),
            param_policy: param_resolution::ParamPolicy::default(),
        }
    }
}

/// Compile Wado source code with a `CompilerHost` for I/O operations.
///
/// This is the main compilation entry point. It runs the full compilation pipeline:
/// lexer -> parser -> binder -> loader -> analyzer -> elaborator -> lower -> optimize -> `tir_to_wir`
///
/// # Arguments
/// * `source` - The entry module source code
/// * `host` - `CompilerHost` for loading imported modules and emitting diagnostics
/// * `filename` - Optional filename for error messages
/// * `opt_level` - Optimization level
///
/// # Example
/// ```ignore
/// let host = FilesystemCompilerHost::new(base_path);
/// let result = compile_with_host(source, &host, Some("main.wado"), OptLevel::O1).await?;
/// ```
pub async fn compile_with_host<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    opt_level: OptLevel,
) -> Result<CompileResult, CompileFailure> {
    let options = CompilerOptions {
        opt_level,
        target_world: None,
        ..CompilerOptions::default()
    };
    compile_with_options(source, host, filename, options).await
}

/// Compile Wado source code with full options.
///
/// This is the main compilation entry point with all options. It runs the full compilation pipeline:
/// lexer -> parser -> binder -> loader -> analyzer -> elaborator -> lower -> optimize -> `tir_to_wir`
///
/// Emit unused-item warnings for the user-authored items the liveness pass
/// classified: `DeadFunction` / `DeadGlobal` for items reachable from neither
/// production nor tests, and `TestOnlyFunction` / `TestOnlyGlobal` for items
/// reachable only from `test` blocks.
fn emit_unused_diagnostics<H: CompilerHost>(
    sem: &semantics::Semantics,
    logger: &Logger<'_, H>,
    is_test_world: bool,
) {
    use crate::ast::Item;
    use crate::compiler_host::{Code, DiagnosticSpan};

    let emit = |ids: &[crate::ast::AstId], fn_code: Code, global_code: Code, reason: &str| {
        for id in ids {
            let Some(owning) = sem.module_of_id(*id) else {
                continue;
            };
            let Some(module) = sem.modules.get(owning) else {
                continue;
            };
            let filename = owning.diagnostic_filename();
            for item in &module.items {
                match item {
                    Item::Function(func) if func.id == *id => {
                        logger.warn_at(
                            fn_code,
                            format!("function `{}` {reason}", func.name),
                            DiagnosticSpan::from_span(&func.name_span, Some(filename.as_str())),
                        );
                    }
                    Item::Global(global) if global.id == *id => {
                        logger.warn_at(
                            global_code,
                            format!("global `{}` {reason}", global.name),
                            DiagnosticSpan::from_span(&global.name_span, Some(filename.as_str())),
                        );
                    }
                    _ => {}
                }
            }
        }
    };

    emit(
        &sem.liveness.dead_items,
        Code::DeadFunction,
        Code::DeadGlobal,
        "is never used",
    );

    // A test-only item is production dead code, but flagging it during a
    // `wado test` run — where the `test` blocks that reach it are the whole
    // point — would be noise. Report it only in non-test builds (`wado
    // compile` / `wado check`).
    if !is_test_world {
        emit(
            &sem.liveness.test_only_items,
            Code::TestOnlyFunction,
            Code::TestOnlyGlobal,
            "is only used by tests",
        );
    }
}

/// Synthesize a library [`WorldInfo`] (`--lib`) from the entry module's
/// `export fn` signatures: one direct world export per exported function.
///
/// Milestone 2 is functions-only and primitives-only, so each export is a
/// direct world function (`from_interface_fq = None`). Parameter and return
/// types are taken straight from the AST signature.
/// The interface FQ a `core:kiln/generator` component's synthesized world uses
/// for `generate` and its options record (Kiln WEP revision 3). A generator's
/// `generate` is grouped into this interface (it references the local `Options`
/// record), and the record type is registered under this FQ.
// TODO(kiln-abi-v3): derive from the generator package's own namespace/name so the
// published component's WIT carries a package-specific identity.
const KILN_GENERATOR_IMPL_FQ: &str = "kiln:generator/generator@0.1.0";

fn synthesize_lib_world_info(
    fq: &str,
    entry_module: Option<&ast::Module>,
) -> world_registry::WorldInfo {
    use crate::ast::Item;
    use crate::world_registry::{WorldExportInfo, WorldInfo};

    let mut exports: Vec<WorldExportInfo> = entry_module
        .map(|module| {
            module
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Function(func) if func.is_export => Some(WorldExportInfo {
                        name: func.name.clone(),
                        is_async: func.is_async,
                        params: func
                            .params
                            .iter()
                            .map(|p| (p.name.clone(), p.ty.clone()))
                            .collect(),
                        return_type: func.return_type.clone(),
                        from_interface_fq: None,
                    }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // A/B grouping (mirrors the WIT producer, `wit_emit`): if any exported
    // signature references a user-defined named type, the exports cannot be
    // bare world functions — a named type reaches consumers only through an
    // exported interface — so they are grouped into the default interface
    // (named after the package, i.e. the library world's own FQ). Otherwise
    // they stay direct world exports.
    let references_named_type = exports.iter().any(|e| {
        e.params.iter().any(|(_, ty)| lib_sig_uses_named_type(ty))
            || e.return_type.as_ref().is_some_and(lib_sig_uses_named_type)
    });
    if references_named_type {
        for export in &mut exports {
            export.from_interface_fq = Some(fq.to_string());
        }
    }

    // Tag the entry module's own named types in the export signatures with the
    // library's default-interface FQ, matching `register_lib_local_decls`, so
    // the lift/lower machinery resolves them like WASI types. Only the module's
    // own declarations — types from other interfaces keep their source.
    let local_type_names: crate::hashmap::IndexSet<String> = entry_module
        .map(|module| {
            module
                .items
                .iter()
                .filter_map(|item| match item {
                    Item::Struct(d) => Some(d.name.clone()),
                    Item::Enum(d) => Some(d.name.clone()),
                    Item::Variant(d) => Some(d.name.clone()),
                    Item::Flags(d) => Some(d.name.clone()),
                    Item::Newtype(d) => Some(d.name.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    for export in &mut exports {
        for (_, ty) in &mut export.params {
            annotate_lib_local_sources(ty, fq, &local_type_names);
        }
        if let Some(ty) = export.return_type.as_mut() {
            annotate_lib_local_sources(ty, fq, &local_type_names);
        }
    }

    WorldInfo {
        fq_name: fq.to_string(),
        exports,
        imports: Vec::new(),
    }
}

/// Set `source_interface = fq` on every user named type in `ty` (recursing
/// through containers), so the CM lift/lower machinery resolves library-local
/// records / variants / enums / flags / newtypes against the package's
/// default-interface registration. CM primitives and the unit type are left
/// untouched.
fn annotate_lib_local_sources(
    ty: &mut ast::Type,
    fq: &str,
    local_type_names: &crate::hashmap::IndexSet<String>,
) {
    use crate::ast::Type;
    match ty {
        Type::Named(named) => {
            // Only untagged, package-local names: an already-resolved type (a
            // shared `core:kiln/types` record) keeps its own interface, which
            // the CM lift/lower needs to find its fields.
            if named.source_interface.is_none() && local_type_names.contains(&named.name) {
                named.source_interface = Some(fq.to_string());
            }
        }
        Type::Generic(g) => {
            for arg in &mut g.args {
                annotate_lib_local_sources(arg, fq, local_type_names);
            }
        }
        Type::Tuple(elems) => {
            for elem in elems {
                annotate_lib_local_sources(elem, fq, local_type_names);
            }
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            annotate_lib_local_sources(inner, fq, local_type_names);
        }
        _ => {}
    }
}

/// Whether a `--lib` export signature type references a user-defined named type
/// (`struct` / `variant` / `enum` / `flags` / type alias), recursing through
/// containers. CM primitives (`bool`, integers, `f32`/`f64`, `char`, `String`)
/// and the unit type are not user types.
fn lib_sig_uses_named_type(ty: &ast::Type) -> bool {
    use crate::ast::Type;
    match ty {
        Type::Named(named) => {
            named.name != "()"
                && crate::component_model::wado_primitive_name_to_cm(&named.name).is_none()
        }
        Type::Generic(g) => g.args.iter().any(lib_sig_uses_named_type),
        Type::Tuple(elems) => elems.iter().any(lib_sig_uses_named_type),
        Type::Reference(inner) | Type::MutReference(inner) => lib_sig_uses_named_type(inner),
        _ => false,
    }
}

/// # Arguments
/// * `source` - The entry module source code
/// * `host` - `CompilerHost` for loading imported modules and emitting diagnostics
/// * `filename` - Optional filename for error messages
/// * `options` - Compilation options including optimization level and target world
pub async fn compile_with_options<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    options: CompilerOptions,
) -> Result<CompileResult, CompileFailure> {
    let log_level = options.log_level.unwrap_or_default();
    let logger = Logger::new(host, log_level);
    let filename = filename.map(String::from);
    if let Some(ref f) = filename {
        logger.set_file(f);
    }

    // === Phase 1: Load all modules ===
    // Loader performs: lex → parse → bind for each module, and preserves
    // the entry AST for tooling that takes it by value.
    let load_result = {
        let module_loader = loader::ModuleLoader::new(host, log_level)
            .with_invocations(options.invocations.clone());
        module_loader
            .load_all(source, filename.as_deref())
            .await
            .map_err(|e| {
                let _ = logger.error(e);
                // Parse failed — cannot determine TODO status
                CompileFailure {
                    is_todo_module: false,
                }
            })?
    };

    // Detect #![TODO] from the entry module AST (available after Phase 1)
    let is_todo_module = load_result.entry_ast.has_todo();

    // Wrap all subsequent Bail errors with is_todo_module
    let result = compile_after_load(load_result, options, &logger, filename);
    match result {
        Ok((wasm, module, wir_package, kiln_options_descriptor)) => Ok(CompileResult {
            wasm,
            module,
            wir_package,
            is_todo_module,
            kiln_options_descriptor,
        }),
        Err(Bail) => Err(CompileFailure { is_todo_module }),
    }
}

/// Internal: run compilation phases after module loading.
fn compile_after_load<H: CompilerHost>(
    load_result: loader::LoadResult,
    options: CompilerOptions,
    logger: &Logger<'_, H>,
    filename: Option<String>,
) -> Result<
    (
        Vec<u8>,
        ast::Module,
        Option<wir::WirPackage>,
        Option<kiln::OptionsDescriptor>,
    ),
    Bail,
> {
    let module_name = filename.unwrap_or_else(|| "module".to_string());
    // Kept for attributing optimizer remarks to the entry file; `module_name`
    // itself is moved into the `Package` before remarks are emitted.
    let entry_filename = module_name.clone();
    let implicit_modules = load_result.implicit_modules.clone();
    let entry_ast = load_result.entry_ast.clone();
    let mut load_result = load_result;

    // === Phase 1a: Kiln generator import-refusal ===
    // Runs before analysis so `wasi:*` imports in a generator package
    // surface as a clean `KilnGeneratorForbiddenImport` rather than getting
    // lost inside effect/type errors further downstream.
    let rejected = kiln::import_check::check_loaded(
        options.target_world.as_deref(),
        &load_result.entry_module_source,
        &load_result.modules,
        logger,
    );
    if rejected > 0 {
        return Err(Bail);
    }

    // === Phase 1b: Kiln `Request<T>` adapter rewrite ===
    kiln::import_check::inject_kiln_request_adapter(
        options.target_world.as_deref(),
        &load_result.entry_module_source,
        &mut load_result.modules,
    );

    // Save wasm asset bytes before `semantics_with_logger` consumes the
    // `LoadResult`. They flow through the package to codegen below.
    let wasm_assets = load_result.wasm_assets.clone();

    // === Phases 2 + 6a + 6b: Analyze + Annotate + Lower TIR ===
    // `semantics_with_logger` performs analyze, type resolution, and
    // body-level TIR lowering. The resulting `Semantics` carries the
    // `TirModule`s the batch compiler needs plus the use→def reference
    // map LSP queries need.
    //
    // `semantics_with_logger` always returns a `Semantics`. For batch
    // compilation we refuse to continue when the pipeline did not fully
    // resolve — the downstream phases assume populated `state` /
    // `tir_modules`. Diagnostics explaining the failure have already
    // been emitted to the host.
    // Batch path: build TIR (reify) for codegen.
    let sem = semantics::semantics_with_logger(load_result, logger, true);
    if !sem.is_complete() {
        return Err(Bail);
    }

    // Source-level unused diagnostics. Reads the liveness computed during
    // `semantics_with_logger`; gated on the option (CLI `--no-unused`).
    if options.unused_diagnostics {
        let is_test_world = options.target_world.as_deref() == Some("test");
        emit_unused_diagnostics(&sem, logger, is_test_world);
    }

    // === Phase 6b: Effect, Stores, and Default-Purity Checks (Design B) ===
    // All three are produced from `Semantics` (AST + recorded facts), not the
    // emitted TIR, so they see every source function regardless of what reify
    // emits and share their logic with the LSP.
    {
        let _span = logger.span("effect-check");
        let diags = effect_check::check_semantics(&sem);
        let had_error = !diags.is_empty();
        for error in diags.effects {
            let _ = logger.error(error);
        }
        for error in diags.stores {
            let _ = logger.error(error);
        }
        for error in diags.purity {
            let _ = logger.error(error);
        }
        if had_error {
            return Err(Bail);
        }
    }

    // === Phase 6c: Kiln `Options` descriptor extraction ===
    // For the `core:kiln/generator` target world, walk the entry module's
    // `pub struct Options` and produce a structural descriptor that the CLI
    // provider caches on disk. Diagnostics from the extractor surface
    // through the host without bailing: a malformed descriptor does not
    // fail the whole compile, so the driver's provisional fallback still
    // produces a valid cache key.
    // A kiln generator is any target world that imports `KilnHost` — the same
    // structural signal cm_binding and codegen use, so all three agree (a
    // string match on `core:kiln/generator` would miss a future generator
    // world with a different FQ).
    let is_kiln_generator = match (options.target_world.as_deref(), sem.world_registry()) {
        (Some(tw), Some(reg)) => reg.world_imports_interface(tw, "KilnHost"),
        _ => false,
    };

    let kiln_options_descriptor = if is_kiln_generator {
        match kiln::extract_options_descriptor(&sem, &sem.entry_module_source) {
            Ok(d) => Some(d),
            Err(diags) => {
                for d in diags {
                    logger.host().emit_diagnostic(d);
                }
                None
            }
        }
    } else {
        None
    };

    // Synthesize a world from the entry module's `export fn` signatures — for
    // `--lib`, and for a kiln generator target (Kiln WEP revision 3), whose
    // `generate` carries its typed options via the same raw-Wado-type path.
    // Done before `sem.modules` is dropped by the destructure below.
    let synth_world_fq: Option<String> = options
        .lib_world
        .clone()
        .or_else(|| is_kiln_generator.then(|| KILN_GENERATOR_IMPL_FQ.to_string()));
    let mut lib_world_info = synth_world_fq
        .as_ref()
        .map(|fq| synthesize_lib_world_info(fq, sem.modules.get(&sem.entry_module_source)));

    if is_kiln_generator && let Some(world) = lib_world_info.as_mut() {
        // Only `generate` is the generator world's contract; a helper
        // `export fn` beside it is not a world export and must not be
        // force-routed through the async binding below.
        world.exports.retain(|e| e.name == "generate");
        let kiln_shared: crate::hashmap::IndexSet<String> =
            kiln::import_check::KILN_SHARED_TYPE_NAMES
                .iter()
                .map(|s| (*s).to_string())
                .collect();
        for export in &mut world.exports {
            // `generate`'s shared `core:kiln/types` records reach analysis
            // without a `source_interface`; stamp their real interface so the
            // CM lift/lower resolves their fields instead of a same-named type
            // elsewhere (`wasi:http`'s `Response`) or an i32 handle.
            for (_, ty) in &mut export.params {
                annotate_lib_local_sources(
                    ty,
                    kiln::import_check::KILN_TYPES_INTERFACE,
                    &kiln_shared,
                );
            }
            if let Some(ty) = export.return_type.as_mut() {
                annotate_lib_local_sources(
                    ty,
                    kiln::import_check::KILN_TYPES_INTERFACE,
                    &kiln_shared,
                );
            }
            // `generate` returns `Result<_, _>` and must lift via `task.return`
            // (the result binding handles nested records/lists); the canon's
            // async-ness follows `is_async` (`sync_lift = !is_async`), so force
            // it — the user writes `fn generate`, not `async fn`.
            export.is_async = true;
        }
    }

    // Capture the entry module so its own named types can be registered into
    // the CM interface registry (cloned before `sem` is destructured below).
    let lib_entry_module = synth_world_fq
        .as_ref()
        .and_then(|_| sem.modules.get(&sem.entry_module_source).cloned());

    let semantics::Semantics {
        entry_module_source,
        symbols,
        state,
        tir_modules,
        interner,
        ..
    } = sem;

    // `is_complete()` was checked above, so the full pipeline ran and `state`
    // is populated.
    let state = state.expect("elaborator state present when is_complete");

    // Move trait_env out of `state.tysys` rather than cloning the `Arc`,
    // so that `Package` is the unique owner. `synthesize` later calls
    // `TraitEnv::extend_with_synthesised`, which `Arc::try_unwrap`s the
    // `trait_env` and **panics** if the `Arc` has more than one strong
    // reference (see `trait_env.rs::extend_with_synthesised`). `TraitEnv`
    // does not implement `Clone`, so a stray clone at this point cannot
    // degrade gracefully — it would surface as a panic deep inside
    // synthesis. The `debug_assert!` below makes that contract loud at
    // the leak site instead of one stage later.
    let world_registry = state.world_registry;
    let mut tysys = state.tysys;

    // For `--lib`, augment the shared stdlib CM registry with the package's own
    // named types. `Arc::make_mut` copies-on-write — the stdlib snapshot still
    // holds a reference — so the shared copy is never mutated; only this
    // compilation's registry gains the local types.
    if let (Some(fq), Some(entry)) = (synth_world_fq.as_ref(), lib_entry_module.as_ref()) {
        std::sync::Arc::make_mut(&mut tysys.cm_interface_registry).register_lib_local_decls(
            entry,
            fq,
            entry_module_source.clone(),
        );
    }

    debug_assert_eq!(
        std::sync::Arc::strong_count(&tysys.trait_env),
        1,
        "Package::new must be the unique `Arc<TraitEnv>` owner; a leftover \
         per-module clone would panic in extend_with_synthesised"
    );
    // `Rc::try_unwrap` for `builtin_registry` is the same uniqueness
    // contract; `BuiltinRegistry` *does* implement `Clone`, so the
    // fallback path is sound but quietly deep-copies — the debug-assert
    // catches the leak before that happens.
    debug_assert_eq!(
        std::rc::Rc::strong_count(&tysys.builtin_registry),
        1,
        "Package::new must be the unique `Rc<BuiltinRegistry>` owner; a \
         leftover per-module clone would silently fall back to a deep clone"
    );
    let builtin_registry =
        std::rc::Rc::try_unwrap(tysys.builtin_registry).unwrap_or_else(|rc| (*rc).clone());
    let package = Package::new(
        entry_module_source,
        tir_modules,
        symbols,
        tysys.trait_env,
        implicit_modules,
        module_name,
        tysys.cm_interface_registry,
        world_registry,
        builtin_registry,
        interner,
    );

    // Apply options to package (must be before synthesis)
    let mut package = package;
    if let Some(world) = options.target_world {
        package.target_world = world;
    }
    // The synthesized world is carried owned on the package (the static
    // registry cannot hold a per-package world). `--lib` also overrides the
    // target world FQ; a kiln generator keeps `core:kiln/generator` as its
    // target (the import-refusal check, provider, and descriptor extraction
    // all key on it) while still routing `generate` through the lib
    // param-types path via `is_lib_world()`.
    if let Some(lib_world) = lib_world_info {
        if !is_kiln_generator {
            package.target_world.clone_from(&lib_world.fq_name);
        }
        package.lib_world_info = Some(lib_world);
    }
    package.skip_validation = options.skip_validation;
    package.test_name_filters = options.test_name_filters;
    package.wasm_assets = wasm_assets;
    package.codegen_flags =
        match codegen_flags::CodegenFlags::parse(&options.codegen_flags, options.opt_level) {
            Ok(flags) => flags,
            Err(flag) => {
                let _ = logger.error(compiler_host::Diagnostic {
                    severity: compiler_host::Severity::Error,
                    code: compiler_host::Code::UnsupportedFeature,
                    message: format!(
                        "unknown codegen flag: `-f {flag}` (supported: `array-copy`, \
                         `branch-hinting`, `bare-asserts`, optionally prefixed with `no-`)"
                    ),
                    span: None,
                });
                return Err(Bail);
            }
        };

    // Select allocator: find the function tagged with #[allocator("...")] matching the
    // chosen mode, set its export_name to "realloc", and clear export_name from all others.
    {
        let allocator_tag = options.allocator.unwrap_or_else(|| {
            // HTTP service worlds default to `freelist` (long-running process,
            // benefits from reclamation). Detection routes through
            // `WorldInfo::has_http_handler_export` so the "is this the HTTP
            // service world?" rule stays in one place.
            let is_http_service = package
                .world_registry
                .get(&package.target_world)
                .is_some_and(crate::world_registry::WorldInfo::has_http_handler_export);
            if package.is_test_world() {
                "debug".to_string()
            } else if is_http_service || package.is_lib_world() {
                // A library is consumed by a long-running host; reclaim memory.
                "freelist".to_string()
            } else {
                "bump".to_string()
            }
        });
        if let Some(alloc_module) = package.tir_modules.get_mut(&ModuleSource::allocator()) {
            let mut found = false;
            for func_rc in &alloc_module.functions {
                let mut func = func_rc.borrow_mut();
                if func.allocator_tag.as_deref() == Some(&*allocator_tag) {
                    func.export_name = Some("realloc".to_string());
                    found = true;
                } else if func.allocator_tag.is_some() {
                    func.export_name = None;
                }
            }
            if !found {
                let _ = logger.error(compiler_host::Diagnostic {
                    severity: compiler_host::Severity::Error,
                    code: compiler_host::Code::UnsupportedFeature,
                    message: format!("unknown allocator: `{allocator_tag}`"),
                    span: None,
                });
                return Err(Bail);
            }
        }
    }

    // Validate target world (test and library worlds are handled specially,
    // not in the static registry)
    if !package.is_test_world()
        && !package.is_lib_world()
        && package.world_registry.get(&package.target_world).is_none()
    {
        let _ = logger.error(compiler_host::Diagnostic {
            severity: compiler_host::Severity::Error,
            code: compiler_host::Code::UnsupportedFeature,
            message: format!("unknown target world: `{}`", package.target_world),
            span: None,
        });
        return Err(Bail);
    }

    // Validate that every required `CompilerItem` was registered by the
    // elaborator. A missing required item is a stdlib bug — every Wado-side
    // declaration that anchors a compiler item must carry the matching
    // `#[compiler_item("...")]` attribute. Surfacing it here means the
    // failure happens at compile time with a clear message, not at the
    // first synthesis call that reaches for the unregistered item.
    {
        let module = package
            .tir_modules
            .values()
            .next()
            .expect("Package always has at least one TIR module after semantics");
        let missing = module
            .type_table
            .borrow()
            .compiler_items()
            .missing_required(&package.target_world);
        if !missing.is_empty() {
            for item in &missing {
                let _ = logger.error(compiler_host::Diagnostic {
                    severity: compiler_host::Severity::Error,
                    code: compiler_host::Code::CompilerItemAttr,
                    message: format!(
                        "required compiler item `{name}` is not registered; \
                         the stdlib must declare a `#[compiler_item(\"{name}\")]` \
                         attribute on the matching {kind} for target world `{world}`",
                        name = item.attr_name(),
                        kind = item.expected_kind(),
                        world = package.target_world,
                    ),
                    span: None,
                });
            }
            return Err(Bail);
        }
    }

    // === Phase 8: Synthesis (Package -> Package) ===
    let package = {
        let _span = logger.span("synthesis");
        synthesis::synthesize(package).map_err(|message| {
            let _ = logger.error(compiler_host::Diagnostic {
                severity: compiler_host::Severity::Error,
                code: compiler_host::Code::UnsupportedFeature,
                message,
                span: None,
            });
            Bail
        })?
    };

    // === Phase 8c: Effect Dispatch Synthesis ===
    // Lowers `WithHandler` / `Resume` and generates per-effect dispatch
    // infrastructure (struct, mut global, dispatch wrappers). Runs after
    // effect-check so that handler-skip semantics are validated against
    // the original `WithHandler` shape.
    let package = {
        let _span = logger.span("effect-dispatch");
        synthesis::effect_dispatch::synthesize_post_check(package).map_err(|message| {
            let _ = logger.error(compiler_host::Diagnostic {
                severity: compiler_host::Severity::Error,
                code: compiler_host::Code::UnsupportedFeature,
                message,
                span: None,
            });
            Bail
        })?
    };

    // === Phase 8d: Link (Package → FlatPackage) ===
    let mut flat = {
        let _span = logger.span("link");
        link::link(package)
    };

    // === Phase 8e: Resolve compile-time parameters (`#[param]`) ===
    // After symbol resolution (so types are known and all globals are
    // flattened for flat-namespace unknown-`-D` detection) and before
    // monomorphize/lower (so a rewritten scalar initializer is eligible for
    // Constant Global Promotion). See `wep-2026-04-26-compile-time-params.md`.
    {
        let _span = logger.span("param-resolution");
        param_resolution::resolve_params(
            &mut flat,
            &options.param_overrides,
            &options.param_policy,
            logger,
        )?;
    }

    // === Phase 9: Monomorphize (FlatPackage → FlatPackage) ===
    {
        let _span = logger.span("monomorphize");
        monomorphize(&mut flat);
    }

    // === Phase 9b: Erase Newtypes and Flags ===
    // After monomorphize (which needs distinct Newtype/Flags types for trait dispatch)
    // and before lower/optimize/codegen (which expect Newtypes → base type; Flags → u32).
    {
        flat.type_table.borrow_mut().erase_newtypes_and_flags();
    }

    // === Phase 10: Lower (FlatPackage → NirPackage) ===
    let nir = {
        let _span = logger.span("lower");
        lower(flat)
    };

    // === Phase 11: Optimize (NirPackage → NirPackage) ===
    let nir = {
        let _span = logger.span("optimize");
        optimize(
            nir,
            options.opt_level,
            options.inline_threshold,
            options.opt_iterations,
            logger,
        )
    };

    // Emit optimizer remarks for residual value-semantic copies that survived
    // the NIR pipeline. NIR is the last IR with per-expression spans; see
    // `remarks` and WEP `wep-2026-06-03-optimizer-remarks.md`. Gated on the log
    // level so the NIR walk is skipped entirely when remarks would be filtered
    // out (the default CLI level is `warn`).
    if logger.would_log(compiler_host::Severity::Info) {
        for remark in remarks::collect_value_copy_remarks(&nir) {
            logger.remark(
                remark.message,
                compiler_host::DiagnosticSpan::from_span(&remark.span, Some(&entry_filename)),
            );
        }
    }

    // === Phase 12: Build WIR (NirPackage → WirPackage) ===
    let mut wir_package = {
        let _span = logger.span("wir_build");
        wir_build::build_wir_package(&nir)
    };

    // Unresolved `Type^Trait::method` calls are unsatisfied trait bounds that
    // escaped the front end; report cleanly and bail instead of trapping. Empty
    // for well-formed programs.
    if !wir_package.trait_bound_violations.is_empty() {
        // Dedup by (call, site) so distinct sites each report their own location.
        let mut seen = crate::hashmap::IndexSet::default();
        for v in &wir_package.trait_bound_violations {
            if seen.insert((v.call_name.clone(), v.span)) {
                let _ = logger.error(compiler_host::Diagnostic {
                    severity: compiler_host::Severity::Error,
                    code: compiler_host::Code::TypeMismatch,
                    message: trait_bound_violation_message(&v.call_name),
                    span: Some(compiler_host::DiagnosticSpan::from_span(
                        &v.span,
                        Some(&entry_filename),
                    )),
                });
            }
        }
        return Err(Bail);
    }

    // === Phase 13: Optimize WIR ===
    {
        let _span = logger.span("wir_optimize");
        wir_optimize::optimize_wir(
            &mut wir_package,
            options.opt_level,
            nir.codegen_flags,
            logger,
        );
    }

    // === Phase 14: Emit Wasm (WirPackage → Wasm component bytes) ===
    let wasm = {
        let _span = logger.span("codegen");
        codegen::emit_wasm(&nir, &wir_package)
    };

    // Return the entry AST for tooling
    Ok((
        wasm,
        entry_ast,
        if options.retain_wir {
            Some(wir_package)
        } else {
            None
        },
        kiln_options_descriptor,
    ))
}

/// Deep-clone TIR modules into a fully independent, frozen view.
///
/// A module's `TypeTable` and its `Rc<RefCell<TirFunction>>`s are shared with
/// later phases that mutate them in place (DCE's `TypeTable::retain` punches
/// holes; monomorphization rewrites body type ids). The `--tir-resolved` dump
/// must be immune to that, so both the table (one clone shared across the
/// snapshot's modules) and every function are cloned into fresh `Rc`s.
fn snapshot_tir_modules(
    modules: &IndexMap<ModuleSource, tir::TirModule>,
) -> IndexMap<ModuleSource, tir::TirModule> {
    let cloned_tt = modules
        .values()
        .next()
        .map(|m| Rc::new(RefCell::new(m.type_table.borrow().clone())));

    modules
        .iter()
        .map(|(k, m)| {
            let mut m = m.clone();
            if let Some(ref tt) = cloned_tt {
                m.type_table = Rc::clone(tt);
            }
            // Deep-clone the shared `Rc<RefCell<TirFunction>>`s so later
            // in-place mutation (monomorphization) can't reach this snapshot.
            m.functions = m
                .functions
                .iter()
                .map(|f| Rc::new(RefCell::new(f.borrow().clone())))
                .collect();
            m.generic_functions = m
                .generic_functions
                .iter()
                .map(|(key, f)| (key.clone(), Rc::new(RefCell::new(f.borrow().clone()))))
                .collect();
            (k.clone(), m)
        })
        .collect()
}

/// Dump compiler internal state (async version).
///
/// This runs the compilation pipeline up through optimization (without code generation)
/// and returns diagnostic information about the internal state.
///
/// Pipeline: lexer -> parser -> bind -> load -> analyze -> resolve -> lower -> link -> optimize
pub async fn dump_with_host<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    opt_level: OptLevel,
) -> Result<DumpResult, Bail> {
    dump_with_host_and_world(
        source,
        host,
        filename,
        opt_level,
        None,
        None,
        None,
        &[],
        &crate::hashmap::IndexMap::default(),
        param_resolution::ParamPolicy::default(),
    )
    .await
}

/// Dump compiler internal state with an explicit target world.
///
/// Like [`dump_with_host`] but allows specifying the target world so that
/// DCE and other world-aware passes produce the correct output.
#[allow(clippy::too_many_arguments)]
pub async fn dump_with_host_and_world<H: CompilerHost>(
    source: &str,
    host: &H,
    filename: Option<&str>,
    opt_level: OptLevel,
    target_world: Option<&str>,
    inline_threshold: Option<usize>,
    opt_iterations: Option<u32>,
    codegen_flags: &[String],
    param_overrides: &crate::hashmap::IndexMap<String, String>,
    param_policy: param_resolution::ParamPolicy,
) -> Result<DumpResult, Bail> {
    let logger = Logger::new(host, compiler_host::LogLevel::default());
    let filename = filename.map(String::from);
    if let Some(ref f) = filename {
        logger.set_file(f);
    }

    // === Phase 1: Lexer ===
    let mut lex_result = {
        let _span = logger.span("lex");
        lexer::lex(source)
    };
    let tokens_for_dump = lex_result.tokens.clone();
    // Batch path is fail-fast: report any recovered lex error before parsing
    // so the wire format keeps the `lexer error: …` prefix.
    if !lex_result.errors.is_empty() {
        let _ = logger.error(lex_result.errors.remove(0));
        return Err(Bail);
    }

    // === Phase 2: Parser ===
    let (ast, trivia) = {
        let _span = logger.span("parse");
        let mut parser = Parser::from_lex(lex_result);
        let ast = parser.parse();
        if let Some(e) = parser.take_errors().into_iter().next() {
            let _ = logger.error(e);
            return Err(Bail);
        }
        let mut trivia = parser.take_trivia();
        comment::populate_trailing(&mut trivia, &ast);
        comment::populate_inner_tail(&mut trivia, &ast);
        (ast, trivia)
    };

    // === Phase 3: Bind ===
    {
        let _span = logger.span("bind");
        let mut binder = Binder::new(&logger);
        binder.bind_module(&ast)?;
    }

    // === Phase 4: Load all modules ===
    let load_result = {
        let module_loader = loader::ModuleLoader::new(host, compiler_host::LogLevel::default());
        module_loader
            .load_all(source, filename.as_deref())
            .await
            .map_err(|e| {
                let _ = logger.error(e);
                Bail
            })?
    };

    // Wrap the loader's interner for sharing across analyze + resolve.
    let interner = std::rc::Rc::new(std::cell::RefCell::new(load_result.interner));

    // === Phase 6: Analyze all modules ===
    let symbols = {
        let _span = logger.span("analyze");
        let mut analyzer = Analyzer::new(&logger).with_interner(interner.clone());
        analyzer.analyze_loaded_modules(
            &load_result.modules,
            &load_result.entry_module_source,
            load_result.implicit_modules.clone(),
        )?;
        analyzer.into_symbols()
    };

    // === Phase 7: Resolve all modules to TIR ===
    // Hand `load_result.included_files` to the elaborator via partial
    // move + `Rc::new`, matching the `semantics_with_logger` pattern.
    // The map can be megabytes for projects that bundle binary assets
    // via `#include_bytes`, and nothing below this line reads
    // `load_result.included_files` again.
    let included_files = std::rc::Rc::new(load_result.included_files);
    let resolve_output = {
        let _span = logger.span("elaborate");
        Elaborator::elaborate_all_modules(
            &symbols,
            &load_result.modules,
            load_result.entry_module_source.clone(),
            &logger,
            included_files,
            load_result.invocations.clone(),
            interner.clone(),
        )
        .ok()
    };
    // Destructure rather than clone the `Arc<TraitEnv>`: keeping a stray
    // reference alive forces `synthesize`'s `Arc::try_unwrap` into the
    // deep-clone fallback. The resolved modules are kept as-is for the
    // `--tir-resolved` dump view; the pipeline runs on its own snapshot
    // (below), so these stay frozen at the resolved stage.
    let (tir_modules_by_source, trait_env): (
        Option<IndexMap<ModuleSource, tir::TirModule>>,
        Option<std::sync::Arc<crate::elaborator::trait_env::TraitEnv>>,
    ) = match resolve_output {
        Some((modules, env)) => (Some(modules), Some(env)),
        None => (None, None),
    };

    // === Phase 7b+8+9+10: Build Package and run remaining phases ===
    // Create Package early so CM binding synthesis runs before monomorphize,
    // matching the compile_with_options pipeline.
    let (monomorphized_tir_text, lowered_nir_text, optimized_package, wir_package) =
        // The pipeline mutates its input in place, so give it an independent
        // snapshot rather than the frozen dump view.
        if let Some(resolved_modules) = tir_modules_by_source
            .as_ref()
            .map(snapshot_tir_modules)
        {
            let module_name = filename.clone().unwrap_or_else(|| "module".to_string());

            let (mut cm_interface_registry, world_registry) =
                component_model::CmInterfaceRegistry::build_from_stdlib();
            // Mirror elaboration's fold so CM imports resolve during WIR build
            // (the dump path rebuilds the registry from the stdlib snapshot).
            // Stdlib modules are never `Wasm`, so an empty stdlib set suffices.
            crate::elaborator::orchestration::fold_component_interfaces(
                &mut cm_interface_registry,
                &load_result.modules,
                &crate::hashmap::IndexSet::default(),
            );

            let temp_type_table = std::cell::RefCell::new(tir::TypeTable::new());
            let mut builtin_registry =
                builtin_registry::BuiltinRegistry::build_from_stdlib(&temp_type_table);
            // Fold in `#[canonical(...)]` declarations from
            // loader-synthesized wasm-asset modules so calls into a
            // wat/wasm asset's exports lower through the same TirImport
            // path as `core:builtin` declarations.
            for (ms, module) in &load_result.modules {
                if matches!(ms, ModuleSource::Wasm { .. }) {
                    builtin_registry.register_wasm_module(module, &temp_type_table);
                }
            }

            let package = Package::new(
                load_result.entry_module_source.clone(),
                resolved_modules,
                symbols.clone(),
                trait_env.expect("trait_env is set when resolve succeeded"),
                load_result.implicit_modules.clone(),
                module_name,
                cm_interface_registry,
                world_registry,
                builtin_registry,
                interner,
            );

            // Apply target world override (must be before synthesis)
            let mut package = package;
            if let Some(world) = target_world {
                package.target_world = world.to_string();
            }
            package.wasm_assets.clone_from(&load_result.wasm_assets);
            package.codegen_flags =
                match codegen_flags::CodegenFlags::parse(codegen_flags, opt_level) {
                    Ok(flags) => flags,
                    Err(flag) => {
                        let _ = logger.error(compiler_host::Diagnostic {
                            severity: compiler_host::Severity::Error,
                            code: compiler_host::Code::UnsupportedFeature,
                            message: format!(
                                "unknown codegen flag: `-f {flag}` (supported: `array-copy`, \
                                 `branch-hinting`, `bare-asserts`, optionally prefixed with `no-`)"
                            ),
                            span: None,
                        });
                        return Err(Bail);
                    }
                };

            // Validate target world (test world is synthetic, not in registry)
            if !package.is_test_world()
                && package.world_registry.get(&package.target_world).is_none()
            {
                let _ = logger.error(compiler_host::Diagnostic {
                    severity: compiler_host::Severity::Error,
                    code: compiler_host::Code::UnsupportedFeature,
                    message: format!("unknown target world: `{}`", package.target_world),
                    span: None,
                });
                return Err(Bail);
            }

            // Synthesis (must run before monomorphize)
            let package = {
                let _span = logger.span("synthesis");
                synthesis::synthesize(package).map_err(|message| {
                    let _ = logger.error(compiler_host::Diagnostic {
                        severity: compiler_host::Severity::Error,
                        code: compiler_host::Code::UnsupportedFeature,
                        message,
                        span: None,
                    });
                    Bail
                })?
            };

            // Effect dispatch synthesis (lowers WithHandler / Resume).
            // Dump path mirrors the main compile pipeline (Phase 8c).
            let package = {
                let _span = logger.span("effect-dispatch");
                synthesis::effect_dispatch::synthesize_post_check(package).map_err(|message| {
                    let _ = logger.error(compiler_host::Diagnostic {
                        severity: compiler_host::Severity::Error,
                        code: compiler_host::Code::UnsupportedFeature,
                        message,
                        span: None,
                    });
                    Bail
                })?
            };

            // Link
            let mut flat = link::link(package);

            // Resolve compile-time parameters (`#[param]`)
            {
                let _span = logger.span("param-resolution");
                param_resolution::resolve_params(
                    &mut flat,
                    param_overrides,
                    &param_policy,
                    &logger,
                )?;
            }

            // Monomorphize
            {
                let _span = logger.span("monomorphize");
                monomorphize(&mut flat);
            }

            // Erase Newtypes and Flags (after monomorphize, before lower)
            flat.type_table.borrow_mut().erase_newtypes_and_flags();

            // Snapshot monomorphized state (only unparse; Debug format is deferred)
            let mono_text = Some(unparse::unparse_flat_package(&flat));

            // Lower (FlatPackage → NirPackage)
            let nir = {
                let _span = logger.span("lower");
                lower(flat)
            };
            // Snapshot lowered state (NIR right after lower, before optimize)
            let lower_text = Some(nir_unparse::unparse_nir_package(&nir));

            // Optimize
            let nir = {
                let _span = logger.span("optimize");
                optimize(nir, opt_level, inline_threshold, opt_iterations, &logger)
            };

            // WIR: Translate optimized NirPackage to WirPackage for inspection.
            let wir_package = Some({
                let mut wir = wir_build::build_wir_package(&nir);
                wir_optimize::optimize_wir(&mut wir, opt_level, nir.codegen_flags, &logger);
                wir
            });

            (mono_text, lower_text, Some(nir), wir_package)
        } else {
            (None, None, None, None)
        };

    Ok(DumpResult {
        source: source.to_string(),
        tokens: tokens_for_dump,
        ast,
        symbols,
        loaded_modules: load_result.modules.keys().cloned().collect(),
        implicit_modules: load_result.implicit_modules.into_iter().collect(),
        entry_module_source: load_result.entry_module_source,
        tir_modules: tir_modules_by_source,
        monomorphized_tir_text,
        lowered_nir_text,
        optimized_package,
        wir_package,
        trivia,
    })
}

/// Format Wado source code.
///
/// Returns the formatted source with canonical formatting.
/// Preserves comments and the `__DATA__` section.
///
/// # Example
/// ```
/// let source = r#"use {println} from "core:cli";
/// fn run() with Stdout { println("Hello!"); }
/// "#;
/// let formatted = wado_compiler::format(source).unwrap();
/// assert!(formatted.contains("use { println }"));
/// ```
pub fn format(source: &str) -> Result<String, CompileError> {
    // Formatting requires a clean parse — the first recovered lex error
    // becomes a `CompileError::Lexer`.
    let lex_result = lexer::lex(source);
    if let Some(e) = lex_result.errors.first() {
        return Err(CompileError::from_lex_error(e, None));
    }
    let mut parser = Parser::from_lex(lex_result);
    // Formatting requires a clean parse: reject the first recovered error.
    let ast = parser.parse();
    if let Some(e) = parser.take_errors().first() {
        return Err(CompileError::from_parse_error(e, None, parser.has_todo()));
    }
    let mut trivia = parser.take_trivia();
    comment::populate_trailing(&mut trivia, &ast);
    comment::populate_inner_tail(&mut trivia, &ast);

    // Unparse (no lowering - preserve high-level constructs)
    let unparser = unparse::Unparser::new().with_trivia(&trivia);
    let formatted = unparser.unparse(&ast);

    // Refuse to emit output that loses a comment (comments are node-attached,
    // so one wedged between tokens can be dropped).
    if let Some(missing) = dropped_comment(source, &formatted) {
        return Err(CompileError::Format {
            message: format!("formatting would drop a comment ({missing})"),
        });
    }
    Ok(formatted)
}

/// A comment present in `before` but missing from `after` (by delimiter+text
/// multiset; `emit_comment` is verbatim so relocation keeps the same key).
fn dropped_comment(before: &str, after: &str) -> Option<String> {
    use crate::hashmap::IndexMap;
    fn delim(kind: comment::CommentKind) -> &'static str {
        match kind {
            comment::CommentKind::Line => "//",
            comment::CommentKind::DocLine => "///",
            comment::CommentKind::ModuleDoc => "//!",
            comment::CommentKind::Block => "/*",
        }
    }
    fn bag(src: &str) -> IndexMap<(&'static str, String), usize> {
        let mut bag = IndexMap::default();
        for c in lexer::lex(src).comments {
            *bag.entry((delim(c.kind), c.text)).or_default() += 1;
        }
        bag
    }
    let after_bag = bag(after);
    for (key, before_count) in bag(before) {
        if before_count > after_bag.get(&key).copied().unwrap_or(0) {
            let (delim, text) = key;
            let snippet: String = text.trim().chars().take(40).collect();
            return Some(format!("`{delim}{snippet}`"));
        }
    }
    None
}

/// Result of parsing a source file (AST + AstId-keyed trivia, no compilation).
///
/// Lexing and parsing are both error-recovering: `ast` always covers the
/// whole input. `lex_errors` and `errors` collect recovered problems in
/// source order; they stay separate so the wire-format diagnostic prefixes
/// (`lexer error:` / `parse error:`) stay accurate. Batch/format/doc
/// callers that need the old fail-fast behavior call
/// [`ParseResult::into_fail_fast`]; the LSP path uses the partial `ast`.
pub struct ParseResult {
    pub ast: ast::Module,
    pub trivia: comment::TriviaMap,
    /// Lexer errors recovered while tokenising, in source order.
    pub lex_errors: Vec<lexer::LexError>,
    /// Parser errors recovered while building the AST, in source order.
    pub errors: Vec<parser::ParseError>,
}

impl ParseResult {
    /// Fail-fast adapter: if lexing or parsing recovered any syntax error,
    /// return the first as a `CompileError`; otherwise yield the result
    /// unchanged. Used by batch compilation, `wado doc`, and the formatter,
    /// which must reject malformed input.
    pub fn into_fail_fast(self) -> Result<ParseResult, CompileError> {
        if let Some(e) = self.lex_errors.first() {
            return Err(CompileError::from_lex_error(e, None));
        }
        if let Some(e) = self.errors.first() {
            return Err(CompileError::from_parse_error(e, None, self.ast.has_todo()));
        }
        Ok(self)
    }
}

/// Resolve every transitive import of `parsed` and return the loaded
/// module set.
///
/// Stage 2 of the compiler frontend: pair this with [`parse`] for stage 1
/// and [`semantics::semantics_of`] for stage 3 (analyze + resolve). The
/// convenience [`semantics::semantics`] wraps all three for callers that
/// don't need to inspect the parsed entry between stages.
///
/// `invocations` redirects bare `use { … } from "<schema>"` clauses to
/// kiln-generated entry modules. Pass [`kiln::InvocationIndex::new`] when
/// the caller has no kiln pipeline to advertise.
pub async fn load<H: CompilerHost>(
    parsed: ParseResult,
    filename: Option<&str>,
    host: &H,
    invocations: kiln::InvocationIndex,
    log_level: LogLevel,
) -> Result<LoadResult, LoadError> {
    let loader = loader::ModuleLoader::new(host, log_level).with_invocations(invocations);
    loader
        .load_all_from_parsed_entry(parsed.ast, filename)
        .await
}

/// Parse a Wado source file into AST and trivia map.
/// This is a lightweight operation that only lexes and parses; both are
/// error-recovering, so the call cannot fail. Lex / parse errors are
/// surfaced via [`ParseResult::lex_errors`] / [`ParseResult::errors`].
pub fn parse(source: &str) -> ParseResult {
    let mut lex_result = lexer::lex(source);
    let lex_errors = std::mem::take(&mut lex_result.errors);
    let mut parser = Parser::from_lex(lex_result);
    let ast = parser.parse();
    let errors = parser.take_errors();
    let mut trivia = parser.take_trivia();
    comment::populate_trailing(&mut trivia, &ast);
    comment::populate_inner_tail(&mut trivia, &ast);
    ParseResult {
        ast,
        trivia,
        lex_errors,
        errors,
    }
}

/// Compilation error with structured location info
#[derive(Debug)]
pub enum CompileError {
    /// I/O error reading source file
    Io { path: String, message: String },
    /// Lexer error with location
    Lexer {
        message: String,
        line: usize,
        column: usize,
        filename: Option<String>,
    },
    /// Parser error with location
    Parser {
        message: String,
        line: usize,
        column: usize,
        filename: Option<String>,
        /// True if the module had `#![TODO]` before the parse error occurred.
        is_todo_module: bool,
    },
    /// Binding error (local name resolution)
    Bind {
        message: String,
        filename: Option<String>,
    },
    /// Semantic analysis error
    Analyzer {
        message: String,
        line: usize,
        column: usize,
        filename: Option<String>,
    },
    /// The formatter would not round-trip the input (e.g. it would drop a
    /// comment). Reported instead of silently emitting lossy output.
    Format { message: String },
}

impl CompileError {
    /// Returns true if the error occurred in a `#![TODO]` module.
    pub fn is_todo_module(&self) -> bool {
        matches!(
            self,
            CompileError::Parser {
                is_todo_module: true,
                ..
            }
        )
    }

    /// Build a `CompileError::Lexer` from a recovered [`lexer::LexError`].
    /// Single projection consulted by every fail-fast site so message /
    /// line / column extraction lives in one place.
    pub fn from_lex_error(e: &lexer::LexError, filename: Option<&str>) -> Self {
        CompileError::Lexer {
            message: e.to_string(),
            line: e.span.line,
            column: e.span.column,
            filename: filename.map(String::from),
        }
    }

    /// Build a `CompileError::Parser` from a recovered
    /// [`parser::ParseError`]. Mirrors [`Self::from_lex_error`] for the parse
    /// fail-fast path; `is_todo_module` comes from the surrounding AST.
    pub fn from_parse_error(
        e: &parser::ParseError,
        filename: Option<&str>,
        is_todo_module: bool,
    ) -> Self {
        CompileError::Parser {
            message: e.message.clone(),
            line: e.span.line,
            column: e.span.column,
            filename: filename.map(String::from),
            is_todo_module,
        }
    }
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileError::Io { path, message } => {
                write!(f, "Error reading '{path}': {message}")
            }
            CompileError::Lexer {
                message,
                line,
                column,
                filename,
            } => {
                if let Some(file) = filename {
                    write!(f, "{file}:{line}:{column}: lexer error: {message}")
                } else {
                    write!(f, "line {line}, column {column}: lexer error: {message}")
                }
            }
            CompileError::Parser {
                message,
                line,
                column,
                filename,
                ..
            } => {
                if let Some(file) = filename {
                    write!(f, "{file}:{line}:{column}: parse error: {message}")
                } else {
                    write!(f, "line {line}, column {column}: parse error: {message}")
                }
            }
            CompileError::Bind { message, filename } => {
                if let Some(file) = filename {
                    write!(f, "{file}: {message}")
                } else {
                    write!(f, "{message}")
                }
            }
            CompileError::Analyzer {
                message,
                line,
                column,
                filename,
            } => {
                if let Some(file) = filename {
                    write!(f, "{file}:{line}:{column}: analysis error: {message}")
                } else if *line > 0 {
                    write!(f, "{line}:{column}: analysis error: {message}")
                } else {
                    write!(f, "analysis error: {message}")
                }
            }
            CompileError::Format { message } => write!(f, "format error: {message}"),
        }
    }
}

impl std::error::Error for CompileError {}

#[cfg(test)]
mod lib_world_tests {
    use super::synthesize_lib_world_info;

    #[test]
    fn synthesizes_one_export_per_export_fn() {
        let src = r#"
export fn id_u32(v: u32) -> u32 { return v; }
fn helper(x: u32) -> u32 { return x; }
export fn id_bool(v: bool) -> bool { return v; }
"#;
        let module = super::parse(src).ast;
        let world = synthesize_lib_world_info("wado:mylib/mylib@0.1.0", Some(&module));

        assert_eq!(world.fq_name, "wado:mylib/mylib@0.1.0");
        assert!(world.imports.is_empty());
        // Only the two `export fn`s become world exports; `helper` is excluded.
        let names: Vec<&str> = world.exports.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["id_u32", "id_bool"]);

        let id_u32 = &world.exports[0];
        assert!(!id_u32.is_async);
        assert!(id_u32.from_interface_fq.is_none());
        assert_eq!(id_u32.params.len(), 1);
        assert_eq!(id_u32.params[0].0, "v");
        assert!(id_u32.return_type.is_some());
    }

    #[test]
    fn empty_when_no_entry_module() {
        let world = synthesize_lib_world_info("wado:x/x@0.1.0", None);
        assert!(world.exports.is_empty());
    }

    #[test]
    fn groups_into_default_interface_when_named_type_referenced() {
        // A signature referencing a user named type (here `Point`, even nested
        // in `List`) forces all exports into the default interface (the B path).
        let src = r#"
pub struct Point { x: f64, y: f64 }
export fn id_point(v: Point) -> Point { return v; }
export fn id_u32(v: u32) -> u32 { return v; }
export fn id_points(v: List<Point>) -> List<Point> { return v; }
"#;
        let module = super::parse(src).ast;
        let world = synthesize_lib_world_info("wado:geo/geo@0.1.0", Some(&module));
        assert!(
            world
                .exports
                .iter()
                .all(|e| e.from_interface_fq.as_deref() == Some("wado:geo/geo@0.1.0")),
            "all exports group into the default interface when any references a named type",
        );
    }

    #[test]
    fn stays_direct_world_exports_for_containers_of_primitives() {
        // Containers of primitives reference no user type, so exports stay bare
        // (the A path) — no default interface.
        let src = r#"
export fn id_list(v: List<u8>) -> List<u8> { return v; }
export fn id_opt(v: Option<String>) -> Option<String> { return v; }
"#;
        let module = super::parse(src).ast;
        let world = synthesize_lib_world_info("wado:c/c@0.1.0", Some(&module));
        assert!(
            world.exports.iter().all(|e| e.from_interface_fq.is_none()),
            "primitive containers do not force an interface",
        );
    }
}
