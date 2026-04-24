//! Kiln generator import-refusal check.
//!
//! When the target world is `core:kiln/generator`, the generator package
//! must not transitively import any WASI interface at the Wado source
//! level. The sandbox guarantee (WEP 2026-04-12, "Design principles" #1)
//! depends on the generator being deterministic: no clocks, randomness,
//! network, filesystem, or environment. This pass catches the obvious
//! case where a generator writes
//! `use { now } from "wasi:clocks"` directly. The `CompilerHost`'s
//! `run_generator` runtime link refuses any `wasi:*` CM import as defense
//! in depth for the transitive case (a stdlib helper pulling in a WASI
//! interface).
//!
//! Runs after Phase 1 (module loading) and before analysis so the
//! diagnostic points at the user's `use` statement with the same span
//! machinery as any other import error.
//!
//! See WEP 2026-04-12 §"M6.5 stage 2".
use crate::ast::{ImplBlock, Item, Module, NamedType, Type};
use crate::compiler_host::{Code, CompilerHost, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::IndexMap;
use crate::logger::Logger;
use crate::name::ModuleSource;
use crate::synthesis::kiln_synth::KILN_GENERATOR_WORLD;
use crate::token::Span;

/// Run the Kiln generator import-refusal check against every loaded
/// module. Returns the count of rejected `use` sites; zero means the
/// generator passed.
pub fn check_loaded<H: CompilerHost>(
    target_world: Option<&str>,
    entry_module: &ModuleSource,
    modules: &IndexMap<ModuleSource, Module>,
    logger: &Logger<'_, H>,
) -> usize {
    if target_world != Some(KILN_GENERATOR_WORLD) {
        return 0;
    }

    let mut count = 0;
    for (source, module) in modules {
        if !should_check(source) {
            continue;
        }
        count += check_module(module, entry_module, source, logger);
    }
    count
}

/// If the target world is `core:kiln/generator` and the entry module
/// declares `pub struct Options`, ensure an `impl Deserialize for Options;`
/// forward-decl is present so the resolver registers the trait impl and
/// `bind_request::<Options>` typechecks. Idempotent: skipped when the
/// user already wrote the impl.
///
/// Runs post-load, pre-annotate so the resolver sees the synthesized
/// `Item::Impl` during its ordinary walk. The injected AST ids come
/// from [`Module::alloc_ast_id`], which extends the parser's dense
/// per-module range — any symbol-lookup machinery keyed on
/// `(ModuleSource, AstId)` still finds the node.
pub fn inject_deserialize_impl(
    target_world: Option<&str>,
    entry_module: &ModuleSource,
    modules: &mut IndexMap<ModuleSource, Module>,
) {
    if target_world != Some(KILN_GENERATOR_WORLD) {
        return;
    }

    let Some(module) = modules.get_mut(entry_module) else {
        return;
    };

    let has_options_struct = module
        .items
        .iter()
        .any(|it| matches!(it, Item::Struct(s) if s.name == "Options"));
    if !has_options_struct {
        return;
    }

    let already_impld = module.items.iter().any(|it| {
        if let Item::Impl(block) = it
            && let Some(tt) = &block.trait_type
            && type_name(tt) == Some("Deserialize")
            && type_name(&block.ty) == Some("Options")
        {
            return true;
        }
        false
    });
    if already_impld {
        return;
    }

    let span = synthesized_span();
    let trait_id = module.alloc_ast_id();
    let target_id = module.alloc_ast_id();
    let impl_id = module.alloc_ast_id();

    let trait_type = Type::Named(NamedType::new(trait_id, "Deserialize".to_string(), span));
    let target_ty = Type::Named(NamedType::new(target_id, "Options".to_string(), span));

    let block = ImplBlock {
        id: impl_id,
        type_params: Vec::new(),
        trait_type: Some(trait_type),
        ty: target_ty,
        associated_types: Vec::new(),
        constants: Vec::new(),
        methods: Vec::new(),
        is_synthesize_request: true,
        span,
    };

    module.items.push(Item::Impl(block));
}

fn type_name(ty: &Type) -> Option<&str> {
    match ty {
        Type::Named(n) => Some(n.name.as_str()),
        Type::Generic(g) => Some(g.name.as_str()),
        _ => None,
    }
}

fn synthesized_span() -> Span {
    Span::new(0, 0, 0, 0)
}

fn should_check(source: &ModuleSource) -> bool {
    match source {
        ModuleSource::EntryPoint { .. }
        | ModuleSource::Local { .. }
        | ModuleSource::Remote { .. } => true,
        ModuleSource::Core { .. } | ModuleSource::Wasi { .. } => false,
    }
}

fn check_module<H: CompilerHost>(
    module: &Module,
    entry_module: &ModuleSource,
    this: &ModuleSource,
    logger: &Logger<'_, H>,
) -> usize {
    let mut count = 0;
    for item in &module.items {
        if let Item::Use(decl) = item
            && let Some(reason) = forbid_reason(&decl.source)
        {
            let which = if this == entry_module {
                "entry module".to_string()
            } else {
                format!("module `{this}`")
            };
            let message = format!(
                "kiln generator `{which}` imports `{src}`: {reason}. \
                 Generators must be deterministic and can only import \
                 `core:*` modules and project-relative paths. \
                 See WEP 2026-04-12 §\"Design principles\".",
                src = decl.source,
            );
            let filename = this.to_string();
            let _ = logger.error(Diagnostic {
                severity: Severity::Error,
                code: Code::KilnGeneratorForbiddenImport,
                message,
                span: Some(DiagnosticSpan::from_span(
                    &decl.source_span,
                    Some(&filename),
                )),
            });
            count += 1;
        }
    }
    count
}

/// Return `Some(reason)` when `source` is not permitted inside a Kiln
/// generator package.
fn forbid_reason(source: &str) -> Option<&'static str> {
    if source.starts_with("wasi:") {
        return Some("WASI interfaces break determinism");
    }
    // Any explicit scheme other than `core:` or a project-relative path.
    // Bare URLs (`http://…`, `https://…`) are rejected on the same grounds.
    if source.starts_with("http:") || source.starts_with("https:") {
        return Some("network imports are not allowed");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forbid_reason_flags_wasi_and_http() {
        assert!(forbid_reason("wasi:clocks").is_some());
        assert!(forbid_reason("wasi:cli/stdout").is_some());
        assert!(forbid_reason("https://example.com/foo.wado").is_some());
        assert!(forbid_reason("http://example.com/foo.wado").is_some());
    }

    #[test]
    fn forbid_reason_allows_core_and_relative() {
        assert!(forbid_reason("core:kiln").is_none());
        assert!(forbid_reason("core:kiln/kiln_host.wado").is_none());
        assert!(forbid_reason("core:prelude").is_none());
        assert!(forbid_reason("core:json").is_none());
        assert!(forbid_reason("./options.wado").is_none());
        assert!(forbid_reason("../shared.wado").is_none());
    }
}
