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
    AstId, Expr, GenericType, IdentExpr, Item, LetStmt, Module, NamedType, Param, Pattern, SelfKind,
    Stmt, StructLiteralExpr, StructLiteralField, Type, UseDecl, UseItem,
};
use crate::compiler_host::{Code, CompilerHost, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::IndexMap;
use crate::logger::Logger;
use crate::module_source::ModuleSource;
use crate::token::Span;

pub const KILN_GENERATOR_WORLD: &str = "core:kiln/generator";

/// The shared CM interface FQ that carries `InputFile` / `Response` / `Error`.
pub const KILN_TYPES_INTERFACE: &str = "core:kiln/types@0.1.0";

/// The `core:kiln/types` record/variant names shared across every generator's
/// `generate` signature. Single source of truth: a `generate` param or return
/// type naming one of these must be stamped with [`KILN_TYPES_INTERFACE`], or
/// the CM lift/lower falls back to an i32 handle. Add a new shared type here.
pub const KILN_SHARED_TYPE_NAMES: &[&str] = &["InputFile", "OutputFile", "Response", "Error"];

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

/// Rewrite a kiln generator's `fn generate(req: Request<T>) -> Result<...>`
/// into the typed-options wire shape (Kiln WEP "Protocol revision v0.3"):
/// the single `req` parameter is replaced by `primary: InputFile`, `inputs:
/// List<InputFile>`, and `options: T`, and the body starts with `let req =
/// Request { primary, inputs, options };`. The user's subsequent body keeps
/// seeing `req` as `Request<T>` via same-scope shadowing (WEP 2026-03-25),
/// so the UX is "author writes `fn generate(req: Request<Options>)`".
///
/// Options cross the boundary as a typed WIT value — not a CBOR blob — so
/// there is no `bind_request` / `RawRequest`. A no-`Options` generator
/// (`T = NoOptions`) omits the `options` parameter entirely (an empty record
/// has no Component Model representation) and its `Request` is built with a
/// literal `NoOptions {}`.
///
/// Gated on `target_world == core:kiln/generator`. Fires when the first
/// param's type is syntactically `Request<T>` (one type argument) or the
/// bare `Request` (a no-options generator), in which case `T` is the
/// default `NoOptions`.
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
    // extract the `Request<T>` inner type (or `NoOptions` for the bare
    // `Request` form). Iterates by index so the mutation pass can bypass
    // Rust's borrow checker when it needs to call `module.alloc_ast_id()`.
    let mut target: Option<(usize, String, RequestOptions)> = None;
    for (idx, item) in module.items.iter().enumerate() {
        let Item::Function(func) = item else { continue };
        if !func.is_export || func.name != "generate" {
            continue;
        }
        let Some(first) = func.params.first() else {
            continue;
        };
        let Some(request_options) = request_options_type(&first.ty) else {
            continue;
        };
        target = Some((idx, first.name.clone(), request_options));
        break;
    }

    let Some((item_idx, param_name, request_options)) = target else {
        return;
    };

    let has_options = matches!(request_options, RequestOptions::Explicit(_));
    let options_type = match request_options {
        RequestOptions::Explicit(ty) => ty,
        RequestOptions::Default => {
            Type::Named(NamedType::new(module.alloc_ast_id(), "NoOptions".to_string(), span))
        }
    };

    // Stamp `InputFile` with its shared `core:kiln/types` source so downstream
    // CM resolution (and the world-synthesis local-type annotation) treats it as
    // that interface's type, not a generator-local one — otherwise it falls back
    // to an i32 handle when lifted as a `List<InputFile>` element.
    let input_file_ty = |module: &mut Module| {
        let mut named = NamedType::new(module.alloc_ast_id(), "InputFile".to_string(), span);
        named.source_interface = Some(KILN_TYPES_INTERFACE.to_string());
        Type::Named(named)
    };
    let param = |module: &mut Module, name: &str, ty: Type| Param {
        id: module.alloc_ast_id(),
        name: name.to_string(),
        name_span: span,
        ty,
        self_kind: SelfKind::None,
        is_mut: false,
        default: None,
        span,
    };

    let primary_ty = input_file_ty(module);
    let primary_param = param(module, "primary", primary_ty);
    let inputs_elem = input_file_ty(module);
    let inputs_ty = Type::Generic(GenericType {
        id: module.alloc_ast_id(),
        name: "List".to_string(),
        args: vec![inputs_elem],
        span,
    });
    let inputs_param = param(module, "inputs", inputs_ty);
    let options_param = has_options.then(|| param(module, "options", options_type));

    // `Request { primary, inputs, options }`, where `options` is the typed
    // `options` argument (or a literal `NoOptions {}` for a no-options
    // generator). Same-scope shadowing rebinds `<param_name>` to `Request<T>`.
    let ident_field = |module: &mut Module, name: &str| StructLiteralField {
        name: name.to_string(),
        name_id: module.alloc_ast_id(),
        name_span: span,
        value: Expr::Ident(IdentExpr {
            id: module.alloc_ast_id(),
            name: name.to_string(),
            span,
            segments: Vec::new(),
            type_args: Vec::new(),
        }),
        is_shorthand: false,
        span,
    };
    let primary_field = ident_field(module, "primary");
    let inputs_field = ident_field(module, "inputs");
    let options_field = if has_options {
        ident_field(module, "options")
    } else {
        StructLiteralField {
            name: "options".to_string(),
            name_id: module.alloc_ast_id(),
            name_span: span,
            value: Expr::StructLiteral(Box::new(StructLiteralExpr {
                id: module.alloc_ast_id(),
                name: Some("NoOptions".to_string()),
                name_id: Some(module.alloc_ast_id()),
                name_span: Some(span),
                fields: Vec::new(),
                spreads: Vec::new(),
                has_trailing_comma: false,
                span,
            })),
            is_shorthand: false,
            span,
        }
    };
    let request_lit = Expr::StructLiteral(Box::new(StructLiteralExpr {
        id: module.alloc_ast_id(),
        name: Some("Request".to_string()),
        name_id: Some(module.alloc_ast_id()),
        name_span: Some(span),
        fields: vec![primary_field, inputs_field, options_field],
        spreads: Vec::new(),
        has_trailing_comma: false,
        span,
    }));
    let let_stmt = Stmt::Let(LetStmt {
        id: module.alloc_ast_id(),
        pattern: Pattern::Ident {
            id: module.alloc_ast_id(),
            name: param_name,
            span,
        },
        name_span: span,
        is_mut: false,
        is_reactive: false,
        ty: None,
        value: Some(request_lit),
        span,
    });

    // The author imported `Request`; the synthesis also needs `InputFile`
    // (and `NoOptions` for a no-options generator).
    let mut needed: Vec<&str> = vec!["InputFile"];
    if !has_options {
        needed.push("NoOptions");
    }
    ensure_kiln_imports(module, span, &needed);

    if let Some(Item::Function(func)) = module.items.get_mut(item_idx) {
        let mut params = vec![primary_param, inputs_param];
        if let Some(options_param) = options_param {
            params.push(options_param);
        }
        func.params = params;
        if let Some(body) = func.body.as_mut() {
            body.stmts.insert(0, let_stmt);
        }
    }
}

/// The options binding implied by a `generate` first-parameter type.
enum RequestOptions {
    /// `Request<t>` — bind the explicit options type `t`.
    Explicit(Type),
    /// The bare `Request` — bind the default `NoOptions`.
    Default,
}

/// Classify a `generate` first-parameter type for the adapter rewrite.
/// Returns `None` for anything that is not a `Request<T>` or a bare `Request`,
/// which the pass leaves untouched.
fn request_options_type(ty: &Type) -> Option<RequestOptions> {
    match ty {
        Type::Generic(g) if g.name == "Request" && g.args.len() == 1 => {
            Some(RequestOptions::Explicit(g.args[0].clone()))
        }
        Type::Named(n) if n.name == "Request" => Some(RequestOptions::Default),
        _ => None,
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
        visibility: crate::ast::Visibility::Private,
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

fn synthesized_span() -> Span {
    Span::new(0, 0, 0, 0)
}

fn should_check(source: &ModuleSource) -> bool {
    match source {
        ModuleSource::EntryPoint { .. }
        | ModuleSource::Local { .. }
        | ModuleSource::Dependency { .. }
        | ModuleSource::Remote { .. }
        | ModuleSource::Redirected { .. } => true,
        ModuleSource::Core { .. } | ModuleSource::Wasi { .. } | ModuleSource::Wasm { .. } => false,
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
        assert!(forbid_reason("core:cbor").is_none());
        assert!(forbid_reason("./options.wado").is_none());
        assert!(forbid_reason("../shared.wado").is_none());
    }
}
