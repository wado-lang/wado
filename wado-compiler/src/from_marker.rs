//! `impl From<P> for V;` written out as the impl it stands for:
//!
//! ```wado
//! impl From<P> for V {
//!     fn from($value: P) -> Self { return builtin::wrap_in_case::<Self, P>($value); }
//! }
//! ```
//!
//! Once written out, the marker is an ordinary impl: generic over the impl's
//! parameters, found by a `V: From<P>` bound, monomorphized per instance. The
//! builtin picks the one case of `V` carrying a `P` and reports a marker with
//! none, or with more than one.
//!
//! It runs between the symbol table and reference resolution: the marker's
//! trait is known by the declaration it names, and the method it adds is
//! resolved with the rest of the module.

use crate::ast::{
    AstId, Block, CallExpr, EffectsSource, Expr, Function, IdentExpr, ImplBlock, Item, Module,
    NamedType, Param, PathSegment, ReturnKeyword, ReturnStmt, SelfKind, Stmt, Type, Visibility,
};
use crate::compiler_item::{CompilerItem, parse_compiler_item_attrs};
use crate::defs::DefTable;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::name::INTERNAL_PREFIX;
use crate::resolve::TraitHeads;
use crate::symbol::SymbolTable;
use crate::token::Span;

/// The builtin each written-out marker calls.
pub(crate) const WRAP_IN_CASE: &str = "wrap_in_case";

/// Write out every marker whose trait is the compiler's `From`, however the
/// module spells it.
pub(crate) fn write_out_from_markers(
    modules: &mut IndexMap<ModuleSource, Module>,
    symbols: &SymbolTable,
) {
    let markers = from_markers(modules, symbols);
    for (source, at) in markers {
        let module = modules.get_mut(&source).expect("collected from `modules`");
        let Item::Impl(block) = &module.items[at] else {
            unreachable!("collected above")
        };
        let payload = marker_payload(block).expect("collected above").clone();
        let span = block.span;
        let method = from_method(module, &payload, span);
        let Item::Impl(block) = &mut module.items[at] else {
            unreachable!("collected above")
        };
        block.methods.push(method);
        block.is_synthesize_request = false;
    }
}

/// Each `From` marker, by module and item index.
fn from_markers(
    modules: &IndexMap<ModuleSource, Module>,
    symbols: &SymbolTable,
) -> Vec<(ModuleSource, usize)> {
    let candidates: Vec<(&ModuleSource, usize, &Type)> = modules
        .iter()
        .flat_map(|(source, module)| {
            module
                .items
                .iter()
                .enumerate()
                .filter_map(move |(at, item)| match item {
                    Item::Impl(block) if marker_payload(block).is_some() => {
                        Some((source, at, block.trait_type.as_ref()?))
                    }
                    _ => None,
                })
        })
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    let Some(from_decl) = from_trait_decl(modules) else {
        return Vec::new();
    };
    let defs = DefTable::build(modules, symbols);
    let heads = TraitHeads::new(modules, symbols, &defs);
    let from = defs.def_at(from_decl);
    candidates
        .into_iter()
        .filter(|(source, _, trait_type)| heads.resolve(source, trait_type) == Some(from))
        .map(|(source, at, _)| (source.clone(), at))
        .collect()
}

/// The trait declared `#[compiler_item("from")]`.
fn from_trait_decl(modules: &IndexMap<ModuleSource, Module>) -> Option<AstId> {
    modules
        .values()
        .flat_map(|m| &m.items)
        .find_map(|item| match item {
            Item::Trait(decl)
                if parse_compiler_item_attrs(&decl.attrs)
                    .0
                    .contains(&CompilerItem::From) =>
            {
                Some(decl.id)
            }
            _ => None,
        })
}

/// `P` of a bodyless `impl Trait<P> for V;`, whichever trait it names.
fn marker_payload(block: &ImplBlock) -> Option<&Type> {
    if !block.is_synthesize_request {
        return None;
    }
    match block.trait_type.as_ref()? {
        Type::Generic(g) if g.args.len() == 1 => g.args.first(),
        Type::NamespacedGeneric(g) if g.args.len() == 1 => g.args.first(),
        _ => None,
    }
}

/// `fn from($value: P) -> Self { return builtin::wrap_in_case::<Self, P>($value); }`,
/// every node at the marker's span, so what is reported against it points
/// there.
fn from_method(module: &mut Module, source: &Type, span: Span) -> Function {
    let mut fresh = || module.alloc_ast_id();
    let self_type = |fresh: &mut dyn FnMut() -> AstId| {
        Type::Named(NamedType::new(fresh(), "Self".to_string(), span))
    };
    let param_type = source.refreshed(&mut fresh);
    let turbofish = vec![self_type(&mut fresh), source.refreshed(&mut fresh)];
    let return_type = self_type(&mut fresh);
    let callee = Expr::Ident(IdentExpr {
        id: fresh(),
        name: format!("builtin::{WRAP_IN_CASE}"),
        span,
        segments: vec![
            PathSegment {
                id: fresh(),
                name: "builtin".to_string(),
                span,
            },
            PathSegment {
                id: fresh(),
                name: WRAP_IN_CASE.to_string(),
                span,
            },
        ],
        type_args: Vec::new(),
        type_args_on_prefix: false,
    });
    let value_name = format!("{INTERNAL_PREFIX}value");
    let value = Expr::Ident(IdentExpr {
        id: fresh(),
        name: value_name.clone(),
        span,
        segments: Vec::new(),
        type_args: Vec::new(),
        type_args_on_prefix: false,
    });
    let call = Expr::Call(Box::new(CallExpr {
        id: fresh(),
        callee,
        type_args: turbofish,
        args: vec![value],
        has_trailing_comma: false,
        span,
    }));
    let body = Block {
        id: fresh(),
        stmts: vec![Stmt::Return(ReturnStmt {
            id: fresh(),
            keyword: ReturnKeyword::Return,
            value: Some(call),
            span,
        })],
        span,
    };
    let param = Param {
        id: fresh(),
        attrs: Vec::new(),
        name: value_name,
        name_span: span,
        ty: param_type,
        self_kind: SelfKind::None,
        is_mut: false,
        default: None,
        span,
    };
    Function {
        id: fresh(),
        name: "from".to_string(),
        name_span: span,
        visibility: Visibility::Private,
        is_export: false,
        export_targets: Vec::new(),
        is_async: false,
        type_params: Vec::new(),
        attrs: Vec::new(),
        params: vec![param],
        params_span: span,
        return_type: Some(return_type),
        effects: Vec::new(),
        effects_source: EffectsSource::Written,
        holds_contracts_of: None,
        body: Some(body),
        span,
    }
}
