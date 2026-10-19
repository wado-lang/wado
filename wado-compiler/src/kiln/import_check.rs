//! Kiln generator import-refusal check: under `core:kiln/generator` the package
//! must import no WASI interface the world does not, the sandbox guarantee
//! (WEP 2026-04-12) resting on the generator being deterministic.
//! `run_generator`'s runtime link refuses any other `wasi:*` CM import as depth.
use std::fmt::Write;

use crate::ast::Visibility;
use crate::ast::{
    AstId, Block, EffectHandlerBinding, Expr, ExprStmt, GenericType, IdentExpr, Item, LetStmt,
    Module, NamedType, Param, Pattern, SelfKind, Stmt, StrLit, StructLiteralExpr,
    StructLiteralField, Type, UnaryExpr, UnaryOp, UseDecl, UseItem, WithHandlerExpr,
};
use crate::compiler_host::{Code, CompilerHost, Diagnostic, DiagnosticSpan, Severity};
use crate::component_model::{CmInterfaceRegistry, SourceInterfaceBatch};
use crate::hashmap::IndexMap;
use crate::logger::Logger;
use crate::module_source::ModuleSource;
use crate::token::Span;
use crate::wir::{ImportEntry, ImportKind};
use crate::world_registry::{WorldInfo, fq_name_package};

pub const KILN_GENERATOR_WORLD: &str = "core:kiln/generator";

/// The shared CM interface FQ that carries `InputFile` / `Response` / `Error`.
pub const KILN_TYPES_INTERFACE: &str = "core:kiln/types@0.1.0";

const WASI_SCHEME: &str = "wasi:";

/// Reject the WASI interfaces the author's own `use` names, blaming that span.
/// Returns the count of rejected sites; zero means the generator passed.
pub fn check_loaded<H: CompilerHost>(
    target_world: Option<&str>,
    entry_module: &ModuleSource,
    modules: &IndexMap<ModuleSource, Module>,
    logger: &Logger<'_, H>,
) -> usize {
    if target_world != Some(KILN_GENERATOR_WORLD) {
        return 0;
    }
    let (_, worlds) = CmInterfaceRegistry::build_from_stdlib();
    let world = worlds
        .get(KILN_GENERATOR_WORLD)
        .expect("the stdlib declares the generator world");
    let provided = provided_wasi_interfaces(world);

    let mut count = 0;
    for (source, module) in modules {
        if !should_check(source) {
            continue;
        }
        count += check_module(module, &provided, entry_module, source, logger);
    }
    count
}

/// Reject the WASI interfaces the planned imports ask the Kiln linker for that
/// the generator world does not import, which is where a `core:*` module
/// reaching WASI on the generator's behalf first shows. Returns the count; zero
/// means the generator links.
pub fn check_cm_imports<H: CompilerHost>(
    generator_world: Option<&WorldInfo>,
    import_plan: &[ImportEntry],
    logger: &Logger<'_, H>,
) -> usize {
    let Some(world) = generator_world else {
        return 0;
    };

    let mut count = 0;
    for entry in import_plan {
        // A shared-types instance bears no function, so it asks the linker for
        // nothing: a generator carrying `wasi:cli/types` still instantiates.
        if !entry.fq.starts_with(WASI_SCHEME)
            || matches!(entry.kind, ImportKind::SharedTypes)
            || world.imports_cm_interface(&entry.fq)
        {
            continue;
        }
        let _ = logger.error(Diagnostic {
            severity: Severity::Error,
            code: Code::KilnGeneratorForbiddenImport,
            message: format!(
                "kiln generator imports `{fq}`, which the generator world does not, so this \
                 component cannot instantiate. A namespace `use` reaches it, or a `core:*` \
                 module does on the generator's behalf. See WEP 2026-04-12 §\"Design principles\".",
                fq = entry.fq,
            ),
            span: None,
        });
        count += 1;
    }
    count
}

/// The WASI interfaces `world` imports, each as its package and the name a
/// `use` spells: `wasi:random/insecure-seed@0.3.0` gives
/// `("random", "InsecureSeed")`.
fn provided_wasi_interfaces(world: &WorldInfo) -> Vec<(&str, &str)> {
    world
        .imports
        .iter()
        .filter_map(|import| {
            let fq = import.cm_interface_fq.as_deref()?;
            fq.starts_with(WASI_SCHEME)
                .then(|| (fq_name_package(fq), import.interface_name.as_str()))
        })
        .collect()
}

/// Rewrite a kiln generator's `fn generate(req: Request<T>)` into the
/// typed-options wire shape: `req` becomes `primary`, `inputs`, `module` and
/// `options`, and the body opens with `let req = Request { primary, … };`,
/// which same-scope shadowing keeps looking like the author's `req`. A
/// `NoOptions` generator omits `options`, an empty record having no CM form.
/// The author's body then runs under `core:kiln`'s `KilnLogSink`, since a
/// generator has no stderr for `core:log`'s default sink.
pub fn inject_kiln_request_adapter(
    target_world: Option<&str>,
    entry_module: &ModuleSource,
    modules: &mut IndexMap<ModuleSource, Module>,
    source_interfaces: &mut SourceInterfaceBatch,
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
    let mut target: Option<(usize, String, Span, RequestOptions)> = None;
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
        target = Some((idx, first.name.clone(), first.span, request_options));
        break;
    }

    // A diagnostic about a parameter the rewrite introduces points at the one
    // the author wrote.
    let Some((item_idx, param_name, param_span, request_options)) = target else {
        return;
    };

    let has_options = matches!(request_options, RequestOptions::Explicit(_));
    let options_type = match request_options {
        RequestOptions::Explicit(ty) => ty,
        RequestOptions::Default => Type::Named(NamedType::new(
            module.alloc_ast_id(),
            "NoOptions".to_string(),
            span,
        )),
    };

    // Without its `core:kiln/types` source, CM resolution takes `InputFile` for a
    // generator-local type and lifts a `List<InputFile>` element as an i32 handle.
    let mut input_file_ty = |module: &mut Module| {
        let named = NamedType::new(module.alloc_ast_id(), "InputFile".to_string(), span);
        source_interfaces.insert(named.id, KILN_TYPES_INTERFACE.to_string());
        Type::Named(named)
    };
    let param = |module: &mut Module, name: &str, ty: Type| Param {
        id: module.alloc_ast_id(),
        attrs: Vec::new(),
        name: name.to_string(),
        name_span: param_span,
        ty,
        self_kind: SelfKind::None,
        is_mut: false,
        default: None,
        span: param_span,
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
    let module_ty = Type::Named(NamedType::new(
        module.alloc_ast_id(),
        "String".to_string(),
        span,
    ));
    let module_param = param(module, "module", module_ty);
    let options_param = has_options.then(|| param(module, "options", options_type));

    // `Request { primary, inputs, module, options }`, where `options` is the typed
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
            type_args_on_prefix: false,
        }),
        is_shorthand: false,
        span,
    };
    let primary_field = ident_field(module, "primary");
    let inputs_field = ident_field(module, "inputs");
    let module_field = ident_field(module, "module");
    let options_field = if has_options {
        ident_field(module, "options")
    } else {
        StructLiteralField {
            name: "options".to_string(),
            name_id: module.alloc_ast_id(),
            name_span: span,
            value: empty_struct_literal(module, "NoOptions", span),
            is_shorthand: false,
            span,
        }
    };
    let request_lit = Expr::StructLiteral(Box::new(StructLiteralExpr {
        id: module.alloc_ast_id(),
        name: Some("Request".to_string()),
        name_id: Some(module.alloc_ast_id()),
        name_span: Some(span),
        type_args: Vec::new(),
        fields: vec![primary_field, inputs_field, module_field, options_field],
        spreads: Vec::new(),
        has_trailing_comma: false,
        span,
    }));
    let let_stmt = Stmt::Let(LetStmt {
        id: module.alloc_ast_id(),
        attrs: Vec::new(),
        pattern: Pattern::Ident {
            id: module.alloc_ast_id(),
            name: param_name,
            span,
        },
        name_span: span,
        is_mut: false,
        ty: None,
        value: Some(request_lit),
        else_block: None,
        span,
    });

    // `with &mut KilnLogSink {} do { <the author's body> }`, the body moved in
    // below once the function is borrowed.
    let sink = Expr::Unary(Box::new(UnaryExpr {
        id: module.alloc_ast_id(),
        op: UnaryOp::MutRef,
        expr: empty_struct_literal(module, "KilnLogSink", span),
        span,
    }));
    let mut with_sink = WithHandlerExpr {
        id: module.alloc_ast_id(),
        handlers: vec![EffectHandlerBinding {
            id: module.alloc_ast_id(),
            effect: None,
            handler: sink,
            span,
        }],
        body: Block {
            id: module.alloc_ast_id(),
            stmts: Vec::new(),
            span,
        },
        do_span: span,
        span,
    };
    let with_stmt_id = module.alloc_ast_id();

    // The author imported `Request`; the synthesis also needs `InputFile`,
    // `KilnLogSink`, and `NoOptions` for a no-options generator.
    let mut needed: Vec<&str> = vec!["InputFile", "KilnLogSink"];
    if !has_options {
        needed.push("NoOptions");
    }
    ensure_kiln_imports(module, span, &needed);

    if let Some(Item::Function(func)) = module.items.get_mut(item_idx) {
        let mut params = vec![primary_param, inputs_param, module_param];
        if let Some(options_param) = options_param {
            params.push(options_param);
        }
        func.params = params;
        if let Some(body) = func.body.as_mut() {
            with_sink.body.stmts = std::mem::take(&mut body.stmts);
            with_sink.body.span = body.span;
            body.stmts = vec![
                let_stmt,
                Stmt::Expr(ExprStmt {
                    id: with_stmt_id,
                    expr: Expr::WithHandler(Box::new(with_sink)),
                    span,
                }),
            ];
        }
    }
}

/// `name {}`, for a struct with no fields.
fn empty_struct_literal(module: &mut Module, name: &str, span: Span) -> Expr {
    Expr::StructLiteral(Box::new(StructLiteralExpr {
        id: module.alloc_ast_id(),
        name: Some(name.to_string()),
        name_id: Some(module.alloc_ast_id()),
        name_span: Some(span),
        type_args: Vec::new(),
        fields: Vec::new(),
        spreads: Vec::new(),
        has_trailing_comma: false,
        span,
    }))
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
                local_span: span,
            });
        }
        return;
    }

    // No existing core:kiln use — synthesize one at the top.
    let decl = UseDecl {
        id: module.alloc_ast_id(),
        attrs: Vec::new(),
        visibility: Visibility::Private,
        is_export: false,
        source: StrLit::new("core:kiln"),
        source_span: span,
        source_id: module.alloc_ast_id(),
        items: missing
            .iter()
            .map(|name| UseItem::Simple {
                id: AstId::fresh(),
                name: (**name).to_string(),
                name_span: span,
                alias: None,
                local_span: span,
            })
            .collect(),
        items_span: Some(span),
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
        ModuleSource::Core { .. } | ModuleSource::Binding { .. } | ModuleSource::Wasm { .. } => {
            false
        }
        ModuleSource::Effect(head) => should_check(head.module()),
    }
}

fn check_module<H: CompilerHost>(
    module: &Module,
    provided_wasi: &[(&str, &str)],
    entry_module: &ModuleSource,
    this: &ModuleSource,
    logger: &Logger<'_, H>,
) -> usize {
    let which = if this == entry_module {
        "entry module".to_string()
    } else {
        format!("module `{this}`")
    };
    let provided = provided_wasi
        .iter()
        .fold(String::new(), |mut out, (package, name)| {
            let _ = write!(out, "`{name}` from `{WASI_SCHEME}{package}`, ");
            out
        });
    let filename = this.to_string();
    let mut count = 0;
    for item in &module.items {
        let Item::Use(decl) = item else {
            continue;
        };
        for (span, reason) in forbidden_sites(decl, provided_wasi) {
            let message = format!(
                "kiln generator `{which}` imports `{src}`: {reason}. \
                 Generators must be deterministic and can only import \
                 `core:*` modules, {provided}and project-relative paths. \
                 See WEP 2026-04-12 §\"Design principles\".",
                src = decl.source,
            );
            let _ = logger.error(Diagnostic {
                severity: Severity::Error,
                code: Code::KilnGeneratorForbiddenImport,
                message,
                span: Some(DiagnosticSpan::from_span(span, Some(&filename))),
            });
            count += 1;
        }
    }
    count
}

/// The sites of `decl` a generator whose world imports `provided_wasi` may not
/// import, each with the reason: the source itself, or an interface it names
/// from a WASI package the world imports only part of. A namespace `use` names
/// none, and the import-plan check answers for what it reaches.
fn forbidden_sites<'a>(
    decl: &'a UseDecl,
    provided_wasi: &[(&str, &str)],
) -> Vec<(&'a Span, String)> {
    if let Some(reason) = forbid_reason(&decl.source, provided_wasi) {
        return vec![(&decl.source_span, reason.to_string())];
    }
    let Some(package) = wasi_package(&decl.source) else {
        return Vec::new();
    };
    decl.items
        .iter()
        .filter_map(|item| match item {
            UseItem::Simple {
                name, name_span, ..
            }
            | UseItem::InterfaceFunctions {
                interface_name: name,
                name_span,
                ..
            } => Some((name, name_span)),
            UseItem::Wildcard | UseItem::Namespace { .. } => None,
        })
        .filter(|(name, _)| !provided_wasi.contains(&(package, name.as_str())))
        .map(|(name, span)| {
            (
                span,
                format!("the generator world does not import `{name}`"),
            )
        })
        .collect()
}

/// Return `Some(reason)` when `source` is not permitted inside a Kiln
/// generator package, whose world imports the WASI interfaces `provided_wasi`.
fn forbid_reason(source: &str, provided_wasi: &[(&str, &str)]) -> Option<&'static str> {
    if let Some(package) = wasi_package(source)
        && !provided_wasi
            .iter()
            .any(|(provided, _)| *provided == package)
    {
        return Some("the generator world imports none of its interfaces");
    }
    // Any explicit scheme other than `core:` or a project-relative path.
    // Bare URLs (`http://…`, `https://…`) are rejected on the same grounds.
    if source.starts_with("http:") || source.starts_with("https:") {
        return Some("network imports are not allowed");
    }
    None
}

/// The package a `wasi:` source names: `random` for `wasi:random` and for
/// `wasi:random/insecure_seed.wado`.
fn wasi_package(source: &str) -> Option<&str> {
    let path = source.strip_prefix(WASI_SCHEME)?;
    Some(path.split_once('/').map_or(path, |(package, _)| package))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROVIDED: &[(&str, &str)] = &[("random", "Insecure"), ("random", "InsecureSeed")];

    #[test]
    fn forbid_reason_flags_unprovided_wasi_and_http() {
        assert!(forbid_reason("wasi:clocks", PROVIDED).is_some());
        assert!(forbid_reason("wasi:cli/stdout", PROVIDED).is_some());
        assert!(forbid_reason("wasi:randomness", PROVIDED).is_some());
        assert!(forbid_reason("https://example.com/foo.wado", PROVIDED).is_some());
        assert!(forbid_reason("http://example.com/foo.wado", PROVIDED).is_some());
    }

    #[test]
    fn forbid_reason_allows_core_provided_wasi_and_relative() {
        assert!(forbid_reason("core:kiln", PROVIDED).is_none());
        assert!(forbid_reason("core:kiln/kiln_host.wado", PROVIDED).is_none());
        assert!(forbid_reason("core:prelude", PROVIDED).is_none());
        assert!(forbid_reason("core:json", PROVIDED).is_none());
        assert!(forbid_reason("core:cbor", PROVIDED).is_none());
        assert!(forbid_reason("wasi:random", PROVIDED).is_none());
        assert!(forbid_reason("wasi:random/insecure_seed.wado", PROVIDED).is_none());
        assert!(forbid_reason("./options.wado", PROVIDED).is_none());
        assert!(forbid_reason("../shared.wado", PROVIDED).is_none());
    }
}
