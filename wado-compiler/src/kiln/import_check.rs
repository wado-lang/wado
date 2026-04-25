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
use crate::ast::{
    AstId, CallExpr, Expr, IdentExpr, ImplBlock, Item, LetStmt, Module, NamedType, Pattern, Stmt,
    TryOpExpr, Type, UseDecl, UseItem,
};
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

/// Rewrite a kiln generator's `fn generate(req: Request<T>) -> Result<...>`
/// so the first parameter becomes `req: RawRequest` and the body starts
/// with `let req = bind_request::<T>(req)?;`. The user's subsequent body
/// keeps seeing `req` as `Request<T>` via same-scope shadowing (WEP
/// 2026-03-25), so the UX is "author writes `fn generate(req:
/// Request<Options>)`".
///
/// Gated on `target_world == core:kiln/generator` and only fires when
/// the first param's type is syntactically `Request<…>` with exactly
/// one type argument. Generator authors who prefer the explicit v1
/// `fn generate(raw: RawRequest)` shape keep working unchanged — this
/// pass is a no-op for them.
pub fn inject_kiln_request_adapter(
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

    let span = synthesized_span();

    // First pass (read-only): locate the matching `fn generate` and
    // extract the `Request<T>` inner type. Iterates by index so the
    // mutation pass can bypass Rust's borrow checker when it needs to
    // call `module.alloc_ast_id()`.
    let mut target: Option<(usize, String, Type)> = None;
    for (idx, item) in module.items.iter().enumerate() {
        let Item::Function(func) = item else { continue };
        if !func.is_export || func.name != "generate" {
            continue;
        }
        let Some(first) = func.params.first() else {
            continue;
        };
        let Type::Generic(generic_ty) = &first.ty else {
            continue;
        };
        if generic_ty.name != "Request" || generic_ty.args.len() != 1 {
            continue;
        }
        target = Some((idx, first.name.clone(), generic_ty.args[0].clone()));
        break;
    }

    let Some((item_idx, param_name, inner_type)) = target else {
        return;
    };

    // Allocate every synthesized AstId up front so `module.items` stays
    // free of mutable borrows when we reach back into the function.
    let raw_ty_id = module.alloc_ast_id();
    let bind_ident_id = module.alloc_ast_id();
    let raw_arg_id = module.alloc_ast_id();
    let call_id = module.alloc_ast_id();
    let try_id = module.alloc_ast_id();
    let let_id = module.alloc_ast_id();
    let pattern_id = module.alloc_ast_id();

    let raw_request_type = Type::Named(NamedType::new(raw_ty_id, "RawRequest".to_string(), span));

    let bind_callee = Expr::Ident(IdentExpr {
        id: bind_ident_id,
        name: "bind_request".to_string(),
        span,
        segments: Vec::new(),
    });
    let raw_arg = Expr::Ident(IdentExpr {
        id: raw_arg_id,
        name: param_name.clone(),
        span,
        segments: Vec::new(),
    });
    let bind_call = Expr::Call(Box::new(CallExpr {
        id: call_id,
        callee: bind_callee,
        type_args: vec![inner_type],
        args: vec![raw_arg],
        has_trailing_comma: false,
        span,
    }));
    let try_expr = Expr::TryOp(Box::new(TryOpExpr {
        id: try_id,
        expr: bind_call,
        span,
    }));
    let let_stmt = Stmt::Let(LetStmt {
        id: let_id,
        pattern: Pattern::Ident {
            id: pattern_id,
            name: param_name,
            span,
        },
        name_span: span,
        is_mut: false,
        is_reactive: false,
        ty: None,
        value: Some(try_expr),
        span,
    });

    // Ensure `bind_request` and `RawRequest` are imported from
    // `core:kiln` (the author only wrote `Request`/`Response`/`Error`;
    // the synthesis needs the other two).
    ensure_kiln_imports(module, span, &["bind_request", "RawRequest"]);

    if let Some(Item::Function(func)) = module.items.get_mut(item_idx) {
        if let Some(first) = func.params.first_mut() {
            first.ty = raw_request_type;
        }
        if let Some(body) = func.body.as_mut() {
            body.stmts.insert(0, let_stmt);
        }
    }
}

/// Ensure every listed item is imported from `core:kiln`. Mutates an
/// existing `use { ... } from "core:kiln"` declaration when present,
/// otherwise inserts a fresh one at the top of `module.items`.
fn ensure_kiln_imports(module: &mut Module, span: Span, needed: &[&str]) {
    let mut missing: Vec<&&str> = needed.iter().collect();
    for item in &module.items {
        let Item::Use(decl) = item else { continue };
        if decl.source != "core:kiln" {
            continue;
        }
        missing.retain(|name| {
            !decl
                .items
                .iter()
                .any(|u| matches!(u, UseItem::Simple { name: n, .. } if n == **name))
        });
    }
    if missing.is_empty() {
        return;
    }

    // Try to extend an existing `use ... from "core:kiln"` first.
    for item in &mut module.items {
        let Item::Use(decl) = item else { continue };
        if decl.source != "core:kiln" {
            continue;
        }
        for name in &missing {
            decl.items.push(UseItem::Simple {
                id: AstId::fresh(),
                name: (**name).to_string(),
                name_span: span,
                alias: None,
            });
        }
        return;
    }

    // No existing core:kiln use — synthesize one at the top.
    let decl = UseDecl {
        id: module.alloc_ast_id(),
        is_pub: false,
        source: "core:kiln".to_string(),
        source_span: span,
        source_id: module.alloc_ast_id(),
        items: missing
            .iter()
            .map(|name| UseItem::Simple {
                id: AstId::fresh(),
                name: (**name).to_string(),
                name_span: span,
                alias: None,
            })
            .collect(),
        attributes: None,
        span,
    };
    module.items.insert(0, Item::Use(decl));
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
        | ModuleSource::Remote { .. }
        | ModuleSource::Redirected { .. } => true,
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
