//! A marker the compiler derives by writing it out as the impl it stands for.
//!
//! `impl From<P> for V;`:
//!
//! ```wado
//! impl From<P> for V {
//!     fn from($value: P) -> Self { return builtin::wrap_in_case::<Self, P>($value); }
//! }
//! ```
//!
//! `impl Waitable for V;`, case by case, a case without a payload being ready
//! without the host:
//!
//! ```wado
//! impl Waitable for V {
//!     fn waitable_id(&self) -> Option<ResourceId> {
//!         return match self { Self::A($member) => $member.waitable_id(), Self::B => null };
//!     }
//! }
//! ```
//!
//! Once written out, the marker is an ordinary impl: generic over the impl's
//! parameters, found by a bound, monomorphized per instance. For `From`, the
//! builtin picks the one case of `V` carrying a `P` and reports a marker with
//! none, or with more than one. `Waitable` is sealed, and the written-out impl
//! is the one a user module may hold.
//!
//! It runs between the symbol table and reference resolution: the marker's
//! trait is known by the declaration it names, and the method it adds is
//! resolved with the rest of the module.

use crate::ast::{
    AstId, Block, CallExpr, EffectsSource, Expr, Function, GenericType, IdentExpr, ImplBlock, Item,
    Literal, LiteralExpr, MatchArm, MatchExpr, MethodCallExpr, Module, NamedType, Param,
    PathSegment, Pattern, ReturnKeyword, ReturnStmt, SelfKind, Stmt, Type, Visibility,
};
use crate::compiler_item::{CompilerItem, parse_compiler_item_attrs};
use crate::defs::DefTable;
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::name::INTERNAL_PREFIX;
use crate::resolve::TraitHeads;
use crate::symbol::SymbolTable;
use crate::token::Span;

/// The builtin each written-out `From` marker calls.
pub(crate) const WRAP_IN_CASE: &str = "wrap_in_case";

/// What a marker is written out from.
enum Derived {
    /// `From<P>`, with `P`.
    From(Type),
    /// `Waitable`, with each case of the variant and whether it carries a
    /// payload.
    Waitable(Vec<(String, bool)>),
}

/// Write out every marker whose trait is the compiler's `From` or `Waitable`,
/// however the module spells it.
pub(crate) fn write_out_markers(
    modules: &mut IndexMap<ModuleSource, Module>,
    symbols: &SymbolTable,
) {
    for (source, at, derived) in derived_markers(modules, symbols) {
        let module = modules.get_mut(&source).expect("collected from `modules`");
        let Item::Impl(block) = &module.items[at] else {
            unreachable!("collected above")
        };
        let span = block.span;
        let method = match &derived {
            Derived::From(payload) => from_method(module, payload, span),
            Derived::Waitable(cases) => waitable_method(module, cases, span),
        };
        let Item::Impl(block) = &mut module.items[at] else {
            unreachable!("collected above")
        };
        block.methods.push(method);
        block.is_synthesize_request = false;
        block.written_out = true;
    }
}

/// Each derived marker, by module and item index.
fn derived_markers(
    modules: &IndexMap<ModuleSource, Module>,
    symbols: &SymbolTable,
) -> Vec<(ModuleSource, usize, Derived)> {
    let candidates: Vec<(&ModuleSource, usize, &ImplBlock)> = modules
        .iter()
        .flat_map(|(source, module)| {
            module
                .items
                .iter()
                .enumerate()
                .filter_map(move |(at, item)| match item {
                    Item::Impl(block) if block.is_synthesize_request => Some((source, at, block)),
                    _ => None,
                })
        })
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    let defs = DefTable::build(modules, symbols);
    let heads = TraitHeads::new(modules, symbols, &defs);
    let from = trait_decl(modules, CompilerItem::From).map(|id| defs.def_at(id));
    let waitable = trait_decl(modules, CompilerItem::Waitable).map(|id| defs.def_at(id));
    candidates
        .into_iter()
        .filter_map(|(source, at, block)| {
            let named = heads.resolve(source, block.trait_type.as_ref()?)?;
            let derived = if Some(named) == from {
                Derived::From(marker_payload(block)?.clone())
            } else if Some(named) == waitable {
                let decl = defs.ast_id(heads.resolve(source, &block.ty)?);
                Derived::Waitable(variant_cases(modules, decl)?)
            } else {
                return None;
            };
            Some((source.clone(), at, derived))
        })
        .collect()
}

/// The trait declared `#[compiler_item(...)]` as `item`.
fn trait_decl(modules: &IndexMap<ModuleSource, Module>, item: CompilerItem) -> Option<AstId> {
    modules
        .values()
        .flat_map(|m| &m.items)
        .find_map(|it| match it {
            Item::Trait(decl) if parse_compiler_item_attrs(&decl.attrs).0.contains(&item) => {
                Some(decl.id)
            }
            _ => None,
        })
}

/// Each case of the variant declared at `decl`, and whether it carries a
/// payload; `None` where `decl` declares no variant.
fn variant_cases(
    modules: &IndexMap<ModuleSource, Module>,
    decl: AstId,
) -> Option<Vec<(String, bool)>> {
    modules
        .values()
        .flat_map(|m| &m.items)
        .find_map(|item| match item {
            Item::Variant(v) if v.id == decl => Some(
                v.cases
                    .iter()
                    .map(|case| (case.name.clone(), case.payload.is_some()))
                    .collect(),
            ),
            _ => None,
        })
}

/// `P` of a bodyless `impl Trait<P> for V;`, whichever trait it names.
fn marker_payload(block: &ImplBlock) -> Option<&Type> {
    match block.trait_type.as_ref()? {
        Type::Generic(g) if g.args.len() == 1 => g.args.first(),
        Type::NamespacedGeneric(g) if g.args.len() == 1 => g.args.first(),
        _ => None,
    }
}

fn ident(id: AstId, name: &str, span: Span) -> Expr {
    Expr::Ident(IdentExpr {
        id,
        name: name.to_string(),
        span,
        segments: Vec::new(),
        type_args: Vec::new(),
        type_args_on_prefix: false,
    })
}

fn named_type(id: AstId, name: &str, span: Span) -> Type {
    Type::Named(NamedType::new(id, name.to_string(), span))
}

/// A private method returning its one statement's value, every node at the
/// marker's span, so what is reported against it points there.
fn method(
    module: &mut Module,
    name: &str,
    params: Vec<Param>,
    return_type: Type,
    value: Expr,
    span: Span,
) -> Function {
    let body = Block {
        id: module.alloc_ast_id(),
        stmts: vec![Stmt::Return(ReturnStmt {
            id: module.alloc_ast_id(),
            keyword: ReturnKeyword::Return,
            value: Some(value),
            span,
        })],
        span,
    };
    Function {
        id: module.alloc_ast_id(),
        name: name.to_string(),
        name_span: span,
        visibility: Visibility::Private,
        is_export: false,
        export_targets: Vec::new(),
        is_async: false,
        type_params: Vec::new(),
        attrs: Vec::new(),
        params,
        params_span: span,
        return_type: Some(return_type),
        effects: Vec::new(),
        effects_source: EffectsSource::Written,
        holds_contracts_of: None,
        body: Some(body),
        span,
    }
}

/// `fn from($value: P) -> Self { return builtin::wrap_in_case::<Self, P>($value); }`.
fn from_method(module: &mut Module, source: &Type, span: Span) -> Function {
    let mut fresh = || module.alloc_ast_id();
    let param_type = source.refreshed(&mut fresh);
    let turbofish = vec![
        named_type(fresh(), "Self", span),
        source.refreshed(&mut fresh),
    ];
    let return_type = named_type(fresh(), "Self", span);
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
    let call = Expr::Call(Box::new(CallExpr {
        id: fresh(),
        callee,
        type_args: turbofish,
        args: vec![ident(fresh(), &value_name, span)],
        has_trailing_comma: false,
        span,
    }));
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
    method(module, "from", vec![param], return_type, call, span)
}

/// `fn waitable_id(&self) -> Option<ResourceId> { return match self { … }; }`.
fn waitable_method(module: &mut Module, cases: &[(String, bool)], span: Span) -> Function {
    let mut fresh = || module.alloc_ast_id();
    let member = format!("{INTERNAL_PREFIX}member");
    let arms = cases
        .iter()
        .map(|(case, has_payload)| {
            let (bindings, body) = if *has_payload {
                let binding = Pattern::Ident {
                    id: fresh(),
                    name: member.clone(),
                    span,
                };
                let call = Expr::MethodCall(Box::new(MethodCallExpr {
                    id: fresh(),
                    receiver: ident(fresh(), &member, span),
                    method: "waitable_id".to_string(),
                    method_id: fresh(),
                    method_span: span,
                    type_args: Vec::new(),
                    args: Vec::new(),
                    has_trailing_comma: false,
                    span,
                }));
                (vec![binding], call)
            } else {
                let null = Expr::Literal(LiteralExpr {
                    id: fresh(),
                    value: Literal::Null,
                    span,
                });
                (Vec::new(), null)
            };
            MatchArm {
                id: fresh(),
                pattern: Pattern::Variant {
                    variant_name: case.clone(),
                    variant_qualifier: Some(named_type(fresh(), "Self", span)),
                    name_id: fresh(),
                    name_span: span,
                    bindings,
                    span,
                },
                guard: None,
                body,
                span,
            }
        })
        .collect();
    let value = Expr::Match(Box::new(MatchExpr {
        id: fresh(),
        expr: ident(fresh(), "self", span),
        arms,
        span,
    }));
    let self_param = Param {
        id: fresh(),
        attrs: Vec::new(),
        name: "self".to_string(),
        name_span: span,
        ty: Type::Reference(Box::new(named_type(fresh(), "Self", span))),
        self_kind: SelfKind::Ref,
        is_mut: false,
        default: None,
        span,
    };
    let return_type = Type::Generic(GenericType {
        id: fresh(),
        name: "Option".to_string(),
        args: vec![named_type(fresh(), "ResourceId", span)],
        span,
    });
    method(
        module,
        "waitable_id",
        vec![self_param],
        return_type,
        value,
        span,
    )
}
