//! A template shape's `$hole_fmt` bridge: hole `k` rendered through the format
//! trait its specifier selects, into a caller's `Formatter`. Runs
//! pre-monomorphize, so the emitted trait calls resolve there.
//!
//! The `Formatter` literal a specifier builds names its fields through
//! [`FormatterField`], checked against the declaration. Its sentinels are
//! repeated in Rust, and pinned by the e2e fixture `template_format_spec.wado`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::compiler_item::{CompilerItem, FormatterField};
use crate::defs::{DefId, DefKind};
use crate::elaborator::trait_env::{
    BlanketBound, BlanketImpl, BlanketParamSource, BlanketReceiver, ImplReceiver, TraitEnv,
};
use crate::elaborator::{InstanceBody, InstanceImpl, InstanceOrder, SolverBridge};
use crate::format_spec::{Align, FormatKind, TemplateFormatSpec};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::name::{FqTraitName, FqTypeName, LocalMethodName, RefKind, hole_fmt_helper_name};
use crate::synthesis::common::{
    field_access, locals_from_params, make_synthetic_free_function, mut_ref_expr, ref_expr,
};
use crate::synthesis::traits::case_index_dispatch;
use crate::tir::{
    CallArg, FunctionRef, MonomorphInfo, ResolvedType, StructDef, TemplateId, TemplateShape,
    TirBlock, TirExpr, TirExprKind, TirFunction, TirModule, TirParam, TirStmt, TirStmtKind,
    TirStruct, TirStructField, TypeId, deref_to_layers,
};
use crate::token::Span;
use crate::type_table::TypeTable;
use crate::{format_spec, tir};

/// Every `core:prelude/format` symbol this synthesiser needs, resolved once
/// through the [`CompilerItem`] registry.
#[derive(Clone, Debug)]
pub(super) struct FormatStdlibNames {
    pub formatter: String,
    /// In the order [`Align`] names them.
    pub alignment_cases: [EnumCase; 3],
}

#[derive(Clone, Debug)]
pub(super) struct EnumCase {
    pub name: String,
    pub index: u32,
}

impl FormatStdlibNames {
    pub fn from_type_table(type_table: &TypeTable) -> Self {
        let items = type_table.compiler_items();
        let case = |item| {
            let (_, _, name, index) = items.require_enum_case(item);
            EnumCase {
                name: name.to_string(),
                index,
            }
        };
        assert_formatter_layout(type_table);
        Self {
            formatter: items.struct_name(CompilerItem::Formatter).to_string(),
            alignment_cases: [
                case(CompilerItem::AlignmentLeft),
                case(CompilerItem::AlignmentCenter),
                case(CompilerItem::AlignmentRight),
            ],
        }
    }

    /// The `Alignment` case for `align`, defaulting to right as the stdlib does.
    pub fn alignment_case(&self, align: Option<Align>) -> &EnumCase {
        match align.unwrap_or(Align::Right) {
            Align::Left => &self.alignment_cases[0],
            Align::Center => &self.alignment_cases[1],
            Align::Right => &self.alignment_cases[2],
        }
    }
}

/// A field reordered on the Wado side would otherwise turn every `Formatter`
/// literal built here into a silently wrong one.
fn assert_formatter_layout(type_table: &TypeTable) {
    let def = type_table.require_compiler_item_def(CompilerItem::Formatter);
    let defs = type_table.defs();
    let declared: Vec<&str> = defs
        .members(def)
        .iter()
        .filter(|m| defs.kind(**m) == DefKind::Field)
        .map(|m| defs.name(*m))
        .collect();
    let expected: Vec<&str> = FormatterField::ALL.iter().map(|f| f.field_name()).collect();
    assert_eq!(
        declared, expected,
        "`Formatter`'s fields no longer match `FormatterField`"
    );
}

/// Mint `$hole_fmt$<shape>(t: &S, index: i32, f: &mut Formatter)` for every
/// template shape in `module`, arm `index` rendering hole `index` through the
/// trait method its specifier selects.
pub fn synthesize_hole_fmt_helpers(
    module: &mut TirModule,
    tt: &Rc<RefCell<TypeTable>>,
    trait_env: &Arc<TraitEnv>,
) {
    let helpers = hole_fmt_helpers(&module.structs, tt, trait_env);
    module
        .functions
        .extend(helpers.into_iter().map(|f| Rc::new(RefCell::new(f))));
}

/// `$hole_fmt` for each closed template shape among `structs`. An open shape
/// gets none: it holds a hole of type `X` by `&`, and the shape `X = i32`
/// reaches holds the `i32` by value, so monomorphization builds one for each
/// concrete shape a substitution reaches.
pub(crate) fn hole_fmt_helpers(
    structs: &[TirStruct],
    tt: &Rc<RefCell<TypeTable>>,
    trait_env: &Arc<TraitEnv>,
) -> Vec<TirFunction> {
    let names = FormatStdlibNames::from_type_table(&tt.borrow());
    structs
        .iter()
        .filter_map(|s| hole_fmt_helper(s, tt, trait_env, &names))
        .collect()
}

fn hole_fmt_helper(
    s: &TirStruct,
    tt: &Rc<RefCell<TypeTable>>,
    trait_env: &Arc<TraitEnv>,
    names: &FormatStdlibNames,
) -> Option<TirFunction> {
    let (struct_type, holes) = {
        let table = tt.borrow();
        let StructDef::Anon(id) = s.def else {
            return None;
        };
        if table.anon_shape_is_open(id) {
            return None;
        }
        let shape = table.template_shape(id)?;
        let struct_type = table.find_struct_type(s.def)?;
        let holes: Vec<HoleFmtArm> = shape
            .holes
            .iter()
            .zip(&s.fields)
            .map(|(hole, field)| HoleFmtArm {
                field_type: field.type_id,
                hole_type: hole.ty,
                spec: hole.spec.as_deref().map(|spec| {
                    format_spec::parse(spec)
                        .expect("the parser rejects a malformed format specifier")
                }),
            })
            .collect();
        (struct_type, holes)
    };
    let ctx = TemplateCtx {
        tt,
        module_src: s.module_source.clone(),
        trait_env,
        names,
    };
    let mut helper = build_hole_fmt_helper(struct_type, &holes, s.span, &ctx);
    helper.module_source = s.module_source.clone();
    // The index is a constant at every site `TemplateHole::fmt` reaches after
    // `members()` folds, so the splice keeps one arm; as a call it would
    // keep the whole dispatch, and its arms are what the threshold
    // refuses.
    helper.inline_hint = tir::InlineHint::Always;
    Some(helper)
}

/// One arm of a shape's `$hole_fmt` helper, at the hole's own index: the
/// field holding it, as the struct types it, and the hole's type and specifier.
struct HoleFmtArm {
    field_type: TypeId,
    hole_type: TypeId,
    spec: Option<TemplateFormatSpec>,
}

/// Build one shape's `$hole_fmt` helper: `t` is local 0, `index` local 1 and
/// `f` local 2; arm `k` renders hole `k`.
fn build_hole_fmt_helper(
    struct_type: TypeId,
    holes: &[HoleFmtArm],
    span: Span,
    ctx: &TemplateCtx,
) -> TirFunction {
    let (ref_struct_type, formatter_type, mut_ref_formatter, mut_ref_string, helper_name) = {
        let mut table = ctx.tt.borrow_mut();
        let formatter_def = table.require_compiler_item_def(CompilerItem::Formatter);
        let formatter_type = table.make_struct(StructDef::Decl(formatter_def));
        let string_type = table.make_compiler_struct(CompilerItem::String);
        (
            table.make_ref(struct_type),
            formatter_type,
            table.make_mut_ref(formatter_type),
            table.make_mut_ref(string_type),
            hole_fmt_helper_name(&table.mangle_type_arg_for_generic(struct_type)),
        )
    };
    let local = |index: u32, name: &str, type_id: TypeId| {
        TirExpr::new(
            TirExprKind::Local {
                index,
                name: name.to_string(),
            },
            type_id,
            span,
        )
    };
    let f_local = || local(2, "f", mut_ref_formatter);
    // The caller's buffer, which a spec-carrying `Formatter` literal is built
    // over.
    let f_buf = || {
        field_access(
            f_local(),
            FormatterField::Buf.index(),
            FormatterField::Buf.field_name(),
            mut_ref_string,
            span,
        )
    };

    let cases: Vec<(String, u32)> = (0..holes.len())
        .map(|k| (TemplateShape::field_name(k), k as u32))
        .collect();
    let dispatch = case_index_dispatch(
        local(1, "index", TypeTable::I32),
        &cases,
        |field_name, index| {
            let hole = &holes[index as usize];
            let field = field_access(
                local(0, "t", ref_struct_type),
                index,
                field_name,
                hole.field_type,
                span,
            );
            let kind = hole
                .spec
                .as_ref()
                .map_or(FormatKind::Display, |spec| spec.kind);
            let formatter = match hole.spec.as_ref().filter(|s| s.needs_formatter_fields()) {
                Some(spec) => mut_ref_expr(
                    build_formatter_literal(&f_buf, formatter_type, spec, ctx.tt, ctx.names, span),
                    mut_ref_formatter,
                    span,
                ),
                None => f_local(),
            };
            let (dispatch, value) = {
                let tt = ctx.tt.borrow();
                let dispatch =
                    interpolation_dispatch_type(hole.hole_type, kind, ctx.trait_env, &tt);
                (
                    dispatch,
                    deref_to_layers(field, tt.ref_depth(dispatch), &tt),
                )
            };
            let stmts = trait_fmt_call(dispatch, value, formatter, kind, span, ctx);
            TirExpr::new(
                TirExprKind::Block(TirBlock::new(stmts, span)),
                TypeTable::UNIT,
                span,
            )
        },
        TypeTable::UNIT,
        span,
    );
    let body = TirBlock::new(vec![TirStmt::new(TirStmtKind::Expr(dispatch), span)], span);
    let param = |name: &str, type_id: TypeId, local_index: u32| TirParam {
        name: name.to_string(),
        type_id,
        local_index,
        is_mut: false,
        is_mut_ref: false,
        span,
    };
    let params = vec![
        param("t", ref_struct_type, 0),
        param("index", TypeTable::I32, 1),
        param("f", mut_ref_formatter, 2),
    ];
    let locals = locals_from_params(&params);
    make_synthetic_free_function(helper_name, params, TypeTable::UNIT, body, locals)
}

/// Read-only context shared across all template-expansion helpers.
struct TemplateCtx<'a> {
    tt: &'a Rc<RefCell<TypeTable>>,
    module_src: ModuleSource,
    trait_env: &'a Arc<TraitEnv>,
    names: &'a FormatStdlibNames,
}

fn int_literal(value: i64, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::IntLiteral {
            value: value.cast_unsigned(),
            repr: value.to_string(),
        },
        TypeTable::I32,
        span,
    )
}

/// The full `Formatter { … }` literal a field-setting specifier asks for,
/// over the `&mut String` `buf_mut_ref` yields.
fn build_formatter_literal(
    buf_mut_ref: &dyn Fn() -> TirExpr,
    formatter_type: TypeId,
    spec: &TemplateFormatSpec,
    tt: &Rc<RefCell<TypeTable>>,
    names: &FormatStdlibNames,
    span: Span,
) -> TirExpr {
    let alignment_type = {
        let def = tt
            .borrow()
            .require_compiler_item_def(CompilerItem::Alignment);
        tt.borrow_mut().make_enum(def)
    };
    let align_case = names.alignment_case(spec.align);
    let fill_char = spec.fill.unwrap_or(if spec.zero_pad { '0' } else { ' ' });

    // Built from `FormatterField::ALL` so the literal cannot drift out of the
    // declared field order — the names and indices are checked against the
    // declaration in `FormatStdlibNames::from_type_table`.
    let fields = FormatterField::ALL
        .iter()
        .map(|field| {
            let value = match field {
                FormatterField::Fill => {
                    TirExpr::new(TirExprKind::CharLiteral(fill_char), TypeTable::CHAR, span)
                }
                FormatterField::Align => TirExpr::new(
                    TirExprKind::EnumConstruct {
                        enum_type: alignment_type,
                        case_index: align_case.index,
                        case_name: align_case.name.clone(),
                    },
                    alignment_type,
                    span,
                ),
                FormatterField::SignPlus => TirExpr::new(
                    TirExprKind::BoolLiteral(spec.sign_plus),
                    TypeTable::BOOL,
                    span,
                ),
                FormatterField::Alternate => TirExpr::new(
                    TirExprKind::BoolLiteral(spec.alternate),
                    TypeTable::BOOL,
                    span,
                ),
                FormatterField::ZeroPad => TirExpr::new(
                    TirExprKind::BoolLiteral(spec.zero_pad),
                    TypeTable::BOOL,
                    span,
                ),
                FormatterField::Width => {
                    int_literal(spec.width.unwrap_or(FormatterField::NO_WIDTH).into(), span)
                }
                FormatterField::Precision => int_literal(
                    spec.precision
                        .unwrap_or(FormatterField::PRECISION_DEFAULT)
                        .into(),
                    span,
                ),
                FormatterField::Indent => int_literal(0, span),
                FormatterField::Buf => buf_mut_ref(),
            };
            TirStructField {
                name: field.field_name().to_string(),
                value,
                field_index: field.index(),
            }
        })
        .collect();

    TirExpr::new(
        TirExprKind::StructLiteral {
            struct_type: formatter_type,
            struct_name: names.formatter.clone(),
            fields,
        },
        formatter_type,
        span,
    )
}

/// The type an interpolation dispatches its format trait on, which the
/// elaborator's hole check asks the trait of too. A reference renders as its
/// referent unless an impl is written on the reference itself; under
/// `Inspect`, `&x` renders as `&42` through the ref blanket.
pub(crate) fn interpolation_dispatch_type(
    type_id: TypeId,
    kind: FormatKind,
    trait_env: &TraitEnv,
    tt: &TypeTable,
) -> TypeId {
    if kind == FormatKind::Inspect {
        return type_id;
    }
    let trait_ = tt
        .compiler_items()
        .trait_def(kind.trait_item())
        .expect("every format kind names a prelude trait");
    let mut dispatch = type_id;
    while let ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) = tt.get(dispatch)
        && !ref_impl_written(trait_env, trait_, dispatch, tt)
    {
        dispatch = *inner;
    }
    dispatch
}

/// Peel a newtype receiver to its base type so a format call targets the
/// inherited impl — a newtype renders its underlying value for every format
/// kind except `Inspect` (which overrides it with the ` as Name` tag). A manual
/// `impl <Trait>` on the newtype stops the peel.
fn peel_transparent_newtype(
    type_id: TypeId,
    kind: FormatKind,
    trait_name: &FqTraitName,
    ctx: &TemplateCtx,
) -> TypeId {
    if kind == FormatKind::Inspect {
        return type_id;
    }
    let tt = ctx.tt.borrow();
    // A reference is not a chain: `&Meters` formats through the ref impl, so
    // the walk — which steps over references — is asked only of a newtype.
    if !matches!(tt.get(type_id), ResolvedType::Newtype { .. }) {
        return type_id;
    }
    written_newtype_link(ctx.trait_env, trait_name, type_id, &tt)
        .unwrap_or_else(|| tt.reflect_structure_head(type_id))
}

/// Unified format trait dispatch: emit the `value.fmt(&mut f)` call,
/// delegating to the Wado-level trait implementation (including blanket impls).
fn trait_fmt_call(
    type_id: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    kind: FormatKind,
    span: Span,
    ctx: &TemplateCtx,
) -> Vec<TirStmt> {
    let (trait_fq, method) = {
        let tt = ctx.tt.borrow();
        let items = tt.compiler_items();
        let item = kind.trait_item();
        (
            items.trait_fq(item),
            items.trait_method_name(item).to_string(),
        )
    };
    let (trait_name, method_name) = (&trait_fq, method.as_str());
    let target = peel_transparent_newtype(type_id, kind, trait_name, ctx);
    let (val, type_id) = if target == type_id {
        (val, type_id)
    } else {
        // A cast to the base is a no-op on the transparent GC representation.
        let cast = TirExpr::new(
            TirExprKind::Cast {
                expr: Box::new(val),
                target_type: target,
            },
            target,
            span,
        );
        (cast, target)
    };

    let MethodCallInfo {
        local_name,
        monomorph_info,
        impl_module,
        template,
    } = method_call_info_for_type(type_id, trait_name, method_name, ctx);
    let mangled = local_name.to_mangled_name();

    let ref_type = ctx.tt.borrow_mut().make_ref(type_id);
    let receiver = ref_expr(val, ref_type, span);

    let call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: impl_module,
                name: mangled,
                template,
                monomorph_info,
                method_info: Some(local_name),
            },
            vec![],
            vec![CallArg::new(fmt, false)],
        ),
        TypeTable::UNIT,
        span,
    );
    vec![TirStmt::new(TirStmtKind::Expr(call), span)]
}

/// All information needed to build a `FunctionRef` for a trait method call on a given type.
struct MethodCallInfo {
    local_name: LocalMethodName,
    monomorph_info: Option<MonomorphInfo>,
    impl_module: ModuleSource,
    template: Option<TemplateId>,
}

/// Build `MethodCallInfo` for a trait method call on `type_id`.
///
/// Combines name mangling, module resolution, and monomorphization metadata into
/// one place. For `Ref(T)` / `MutRef(T)`, this produces the `MonomorphInfo` needed
/// to instantiate the generic blanket impl (`impl<T: Trait> Trait for &T`) — no
/// type-specific logic is needed at the call site.
fn method_call_info_for_type(
    type_id: TypeId,
    trait_name: &FqTraitName,
    method_name: &str,
    ctx: &TemplateCtx,
) -> MethodCallInfo {
    let tt = ctx.tt;
    let resolved = tt.borrow().get(type_id).clone();
    match resolved {
        // An impl written on the reference answers before the blanket does.
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner)
            if !ref_impl_written(
                ctx.trait_env,
                trait_name.called_decl(),
                type_id,
                &tt.borrow(),
            ) =>
        {
            let ref_kind = RefKind::from_resolved(&resolved).expect("ref classify");
            let (local_name, monomorph_info) = ref_blanket_call(
                ref_kind,
                trait_name,
                method_name,
                inner,
                vec![],
                &tt.borrow(),
            );
            MethodCallInfo {
                local_name,
                monomorph_info: Some(monomorph_info),
                impl_module: ModuleSource::format(),
                template: trait_method_template(
                    ctx.trait_env,
                    trait_name,
                    method_name,
                    type_id,
                    &tt.borrow(),
                ),
            }
        }
        _ => {
            let local_name = method_name_for_type(type_id, trait_name, method_name, ctx.tt);
            if let Some(info) = blanket_method_call_info(&local_name, type_id, method_name, ctx) {
                return info;
            }
            let impl_module = trait_impl_module(&local_name, type_id, ctx);
            let template = trait_call_template(
                ctx.trait_env,
                &local_name,
                type_id,
                &impl_module,
                &tt.borrow(),
            );
            MethodCallInfo {
                local_name,
                monomorph_info: None,
                impl_module,
                template,
            }
        }
    }
}

/// The name and instance of a call through the universal `&T` blanket, keyed
/// by its pointee.
pub(crate) fn ref_blanket_call(
    ref_kind: RefKind,
    trait_name: &FqTraitName,
    method_name: &str,
    pointee: TypeId,
    method_type_args: Vec<TypeId>,
    tt: &TypeTable,
) -> (LocalMethodName, MonomorphInfo) {
    let generic =
        LocalMethodName::new_ref(ref_kind, Some(trait_name.clone()), method_name.to_string());
    ref_blanket_instance(generic, pointee, method_type_args, tt)
}

/// [`ref_blanket_call`] for the call `generic` names, which may be marked
/// `unchecked`.
pub(crate) fn ref_blanket_instance(
    generic: LocalMethodName,
    pointee: TypeId,
    method_type_args: Vec<TypeId>,
    tt: &TypeTable,
) -> (LocalMethodName, MonomorphInfo) {
    let generic_name = generic.to_mangled_name();
    let method_type_arg_names: Vec<FqTypeName> = method_type_args
        .iter()
        .map(|&arg| tt.fq_type_name(arg))
        .collect();
    let local_name = generic.with_type_args(&[tt.fq_type_name(pointee)], &method_type_arg_names);
    let monomorph_info = MonomorphInfo {
        generic_name,
        impl_type_args: vec![pointee],
        method_type_args,
        is_blanket: true,
    };
    (local_name, monomorph_info)
}

/// The template `receiver.method()` of `trait_` instantiates: a written block,
/// a blanket, else a newtype's base's; `None` where a derived body answers.
pub(crate) fn trait_method_template(
    trait_env: &TraitEnv,
    trait_name: &FqTraitName,
    method: &str,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<TemplateId> {
    answering_link(trait_env, trait_name, method, receiver, tt).map(|(_, template)| template)
}

/// [`trait_method_template`] with the link of `receiver`'s newtype chain whose
/// answer it is: the receiver, or the base it inherits the answer from. A call
/// names that link, as the elaborator's does.
pub(crate) fn answering_link(
    trait_env: &TraitEnv,
    trait_name: &FqTraitName,
    method: &str,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<(TypeId, TemplateId)> {
    let trait_ = trait_name.called_decl();
    match instance_order(trait_env, trait_name, receiver, tt)? {
        InstanceOrder::One(chosen) => Some((
            chain_link(receiver, chosen.depth, tt),
            instance_template(trait_env, chosen.body, trait_, method)?,
        )),
        InstanceOrder::Nothing => None,
        InstanceOrder::Tied(blocks) => unreachable!(
            "the elaborator accepted `{}: {trait_name}`, which {blocks:?} tie on",
            tt.fq_type_name(receiver)
        ),
    }
}

/// The template `method` of `trait_` instantiates from `body`; `None` where
/// the compiler supplies it.
fn instance_template(
    trait_env: &TraitEnv,
    body: InstanceBody,
    trait_: DefId,
    method: &str,
) -> Option<TemplateId> {
    match body {
        InstanceBody::Block(block) => trait_env.method_template(block, method),
        InstanceBody::Supplied => None,
        InstanceBody::EqFromCmp(block) => Some(
            PairedEq {
                eq: trait_env
                    .trait_method_decl(trait_, "eq")
                    .expect("`Eq` declares `eq`"),
                block,
            }
            .template(),
        ),
    }
}

/// The type a call answered at `depth` along the chain the solver walks from
/// `receiver` is named by. A call through a reference is named by the
/// reference, whichever level of its pointee's chain answers.
fn chain_link(receiver: TypeId, depth: u32, tt: &TypeTable) -> TypeId {
    let (steps, mut level) = match tt.get_unerased(receiver) {
        ResolvedType::Ref(_) | ResolvedType::MutRef(_) if depth.is_multiple_of(2) => {
            return receiver;
        }
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => (depth / 2, *inner),
        _ => (depth, receiver),
    };
    for _ in 0..steps {
        level = match tt.get_unerased(level) {
            ResolvedType::Newtype { base_type, .. } => *base_type,
            ResolvedType::Flags { .. } => TypeTable::U32,
            // Only a newtype over a reference goes on through one.
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => *inner,
            _ => unreachable!("the solver's chain from `{receiver:?}` has no level {depth}"),
        };
    }
    level
}

/// The template a call of `info` on `receiver` instantiates, else the derived
/// body in `home`; `None` while the receiver's head awaits substitution.
pub(crate) fn trait_call_template(
    trait_env: &TraitEnv,
    info: &LocalMethodName,
    receiver: TypeId,
    home: &ModuleSource,
    tt: &TypeTable,
) -> Option<TemplateId> {
    if tt.receiver_head_awaits_substitution(receiver) {
        return None;
    }
    info.trait_name
        .as_ref()
        .and_then(|trait_name| {
            trait_method_template(
                trait_env,
                trait_name,
                info.declared_method_name(),
                receiver,
                tt,
            )
        })
        .map(|template| template.called_by(info))
        .or_else(|| Some(TemplateId::derived(home.clone(), info)))
}

/// The template a call of `info` on the concrete `receiver` instantiates, or
/// `None` where a derived body answers. Only a reference impl keeps the `&`.
pub(crate) fn method_template_at(
    trait_env: &TraitEnv,
    info: &LocalMethodName,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<TemplateId> {
    let receiver = if info.ref_receiver().is_some() {
        tt.innermost_ref(receiver)
    } else {
        tt.peel_refs(receiver)
    };
    match &info.trait_name {
        Some(trait_name) => trait_method_template(
            trait_env,
            trait_name,
            info.declared_method_name(),
            receiver,
            tt,
        ),
        None => inherent_method_template(trait_env, info.declared_method_name(), receiver, tt),
    }
    .map(|template| template.called_by(info))
}

/// The inherent block answering `receiver.method()`, else the one a newtype
/// inherits it from.
fn inherent_method_template(
    trait_env: &TraitEnv,
    method: &str,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<TemplateId> {
    trait_env
        .answering_template(
            &tt.impl_receiver_key(receiver),
            None,
            &[],
            method,
            |block| tt.impl_reaches_instance(block, receiver),
        )
        .or_else(|| match tt.get(receiver) {
            ResolvedType::Newtype { base_type, .. } => {
                inherent_method_template(trait_env, method, *base_type, tt)
            }
            _ => None,
        })
}

/// Whether a written impl of `trait_` with methods reaches `receiver` itself.
pub(crate) fn written_impl_reaches(
    trait_env: &TraitEnv,
    trait_: DefId,
    receiver: TypeId,
    tt: &TypeTable,
) -> bool {
    written_impl_reaches_where(trait_env, trait_, receiver, tt, |_| true)
}

/// Whether an impl written for this reference's own referent (`&R`) reaches
/// `receiver`, which the universal `&T` blanket does not count as. Format
/// desugaring asks it during elaboration, before the solver joins the
/// environment, so it cannot ask the order.
fn ref_impl_written(trait_env: &TraitEnv, trait_: DefId, receiver: TypeId, tt: &TypeTable) -> bool {
    written_impl_reaches_where(trait_env, trait_, receiver, tt, |block| {
        trait_env.blanket_of_block(block).is_none()
    })
}

fn written_impl_reaches_where(
    trait_env: &TraitEnv,
    trait_: DefId,
    receiver: TypeId,
    tt: &TypeTable,
    admits: impl Fn(DefId) -> bool,
) -> bool {
    trait_env
        .methodful_impls_by_receiver(&tt.impl_receiver_key(receiver), trait_)
        .any(|block| admits(block) && tt.impl_reaches_instance(block, receiver))
}

/// The trait's `eq` emitted into the block of a `cmp` written alone.
#[derive(Clone, Copy)]
pub(crate) struct PairedEq {
    pub(crate) eq: DefId,
    pub(crate) block: DefId,
}

impl PairedEq {
    pub(crate) fn template(self) -> TemplateId {
        TemplateId::in_block(self.eq, self.block)
    }
}

/// The `eq` that a `cmp` written alone gives `instance`, when `trait_` is
/// `Eq`. It is the instance's own, as a written one would be, so a newtype
/// answers `==` with it rather than with its base's.
pub(crate) fn eq_from_written_cmp(
    trait_env: &TraitEnv,
    solver: &SolverBridge,
    trait_: DefId,
    instance: TypeId,
    tt: &TypeTable,
) -> Option<PairedEq> {
    if tt.compiler_items().trait_def(CompilerItem::Eq) != Some(trait_) {
        return None;
    }
    let (CompilerItem::Ord, block) = solver.comparison_written_alone(tt, instance)? else {
        return None;
    };
    let eq = trait_env
        .trait_method_decl(trait_, "eq")
        .expect("`Eq` declares `eq`");
    Some(PairedEq { eq, block })
}

/// Whether `type_id` is one of the five reflection kinds, i.e. whether a
/// `Reflect*`-bounded blanket can claim it.
pub(crate) fn has_reflect_kind(type_id: TypeId, tt: &TypeTable) -> bool {
    tt.reflect_kind(type_id).is_some()
}

/// The value blanket the order selects for `receiver`, at whichever level of
/// its newtype chain answers (WEP 2026-09-01).
pub(crate) fn ranked_value_blanket<'a>(
    trait_env: &'a TraitEnv,
    trait_name: &FqTraitName,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<&'a BlanketImpl> {
    value_blanket_link(trait_env, trait_name, receiver, tt).map(|(blanket, _)| blanket)
}

/// [`ranked_value_blanket`] with the link of `receiver`'s chain it answers.
fn value_blanket_link<'a>(
    trait_env: &'a TraitEnv,
    trait_name: &FqTraitName,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<(&'a BlanketImpl, TypeId)> {
    ranked_blanket(trait_env, trait_name, receiver, tt)
        .filter(|(blanket, _)| blanket.receiver == BlanketReceiver::Value)
}

/// The `&T` blanket the order selects for the reference `receiver` itself.
pub(crate) fn ranked_ref_blanket<'a>(
    trait_env: &'a TraitEnv,
    trait_name: &FqTraitName,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<&'a BlanketImpl> {
    ranked_blanket(trait_env, trait_name, receiver, tt)
        .filter(|(blanket, link)| {
            matches!(blanket.receiver, BlanketReceiver::Ref { .. }) && *link == receiver
        })
        .map(|(blanket, _)| blanket)
}

/// The blanket the order selects for `receiver`, with the link of its chain
/// the blanket answers.
fn ranked_blanket<'a>(
    trait_env: &'a TraitEnv,
    trait_name: &FqTraitName,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<(&'a BlanketImpl, TypeId)> {
    let InstanceOrder::One(InstanceImpl {
        body: InstanceBody::Block(block),
        depth,
    }) = instance_order(trait_env, trait_name, receiver, tt)?
    else {
        return None;
    };
    let blanket = trait_env.blanket_of_block(block)?;
    Some((blanket, chain_link(receiver, depth, tt)))
}

/// What the order selects for `trait_name`, its arguments included, on
/// `receiver`.
fn instance_order(
    trait_env: &TraitEnv,
    trait_name: &FqTraitName,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<InstanceOrder> {
    trait_env
        .solver()
        .instance_order(tt, receiver, trait_name.canonical()?, trait_name.args())
}

/// The link of `receiver`'s chain whose written impl of `trait_name` the order
/// selects, with its depth: the receiver itself at depth 0.
fn written_impl_link(
    trait_env: &TraitEnv,
    trait_name: &FqTraitName,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<(TypeId, u32)> {
    let InstanceOrder::One(InstanceImpl { body, depth }) =
        instance_order(trait_env, trait_name, receiver, tt)?
    else {
        return None;
    };
    let written = match body {
        InstanceBody::Block(block) => trait_env.blanket_of_block(block).is_none(),
        InstanceBody::EqFromCmp(_) => true,
        InstanceBody::Supplied => false,
    };
    written.then(|| (chain_link(receiver, depth, tt), depth))
}

/// Whether the order answers `receiver: trait_name` with an impl written for
/// `receiver` itself, or the `Eq` a `cmp` it writes alone gives it.
pub(crate) fn written_impl_at(
    trait_env: &TraitEnv,
    trait_name: &FqTraitName,
    receiver: TypeId,
    tt: &TypeTable,
) -> bool {
    written_impl_link(trait_env, trait_name, receiver, tt).is_some_and(|(_, depth)| depth == 0)
}

/// The newtype link at or below `receiver` whose written impl of `trait_name`
/// the order selects.
pub(crate) fn written_newtype_link(
    trait_env: &TraitEnv,
    trait_name: &FqTraitName,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<TypeId> {
    written_impl_link(trait_env, trait_name, tt.peel_refs(receiver), tt)
        .map(|(link, _)| link)
        .filter(|&link| matches!(tt.get_unerased(link), ResolvedType::Newtype { .. }))
}

/// The reflection trait `bound` names, or `None` for any other bound.
fn reflect_bound_item(bound: &BlanketBound, tt: &TypeTable) -> Option<CompilerItem> {
    tt.compiler_items()
        .trait_among(bound.decl()?, &CompilerItem::REFLECT_KINDS)
}

/// Whether a blanket derives over reflection: one of its receiver-param bounds
/// is a `Reflect*` trait.
pub(crate) fn blanket_is_reflect_keyed(bounds: &[BlanketBound], tt: &TypeTable) -> bool {
    bounds
        .iter()
        .any(|bound| reflect_bound_item(bound, tt).is_some())
}

/// The blanket a `type_id.trait::method()` call dispatches to where the order
/// selects one at the receiver itself, with the blanket's module and method
/// template. One a newtype inherits from its base is called by the base's name,
/// which [`answering_link`] names.
pub(crate) fn blanket_dispatch_for(
    trait_env: &TraitEnv,
    type_id: TypeId,
    trait_name: &FqTraitName,
    method_name: &str,
    tt: &mut TypeTable,
) -> Option<(MonomorphInfo, ModuleSource, TemplateId)> {
    let (blanket, link) = value_blanket_link(trait_env, trait_name, type_id, tt)?;
    if link != type_id {
        return None;
    }
    let blanket_module = blanket.module.clone();
    let generic_name = LocalMethodName::new(
        blanket.receiver_binder(tt.defs()),
        Some(trait_name.clone()),
        method_name.to_string(),
    )
    .to_mangled_name();
    let impl_type_args = blanket_impl_args(trait_env, blanket, type_id, tt)?;
    let template = trait_env
        .method_template(blanket.def, method_name)
        .unwrap_or_else(|| panic!("a blanket of `{trait_name}` provides no `{method_name}`"));
    Some((
        MonomorphInfo {
            generic_name,
            impl_type_args,
            method_type_args: vec![],
            is_blanket: true,
        },
        blanket_module,
        template,
    ))
}

/// The type args a value blanket's instance keys on for `receiver`, each projection
/// recorded on it; `None` where one does not resolve, so the blanket does not apply.
pub(crate) fn blanket_impl_args(
    trait_env: &TraitEnv,
    blanket: &BlanketImpl,
    receiver: TypeId,
    tt: &mut TypeTable,
) -> Option<Vec<TypeId>> {
    // A blanket's slots are its parameters in declaration order, so a
    // source's position is its slot.
    let sources = trait_env.blanket_param_sources(blanket);
    let mut slots: IndexMap<u32, TypeId> = sources
        .iter()
        .zip(0..)
        .filter(|(source, _)| matches!(source, BlanketParamSource::Receiver))
        .map(|(_, slot)| (slot, receiver))
        .collect();
    tt.project_impl_slots(blanket.def, &mut slots);
    sources
        .iter()
        .zip(0..)
        .map(|(source, slot)| match source {
            BlanketParamSource::Unresolved => None,
            BlanketParamSource::Receiver | BlanketParamSource::Projection => {
                slots.get(&slot).copied()
            }
        })
        .collect()
}

/// The call through the value blanket providing `local_name` where no written
/// impl does, as the ref-blanket arms build theirs.
fn blanket_method_call_info(
    local_name: &LocalMethodName,
    type_id: TypeId,
    method_name: &str,
    ctx: &TemplateCtx,
) -> Option<MethodCallInfo> {
    let trait_name = local_name.trait_name.as_ref()?;
    let (monomorph_info, blanket_module, template) = blanket_dispatch_for(
        ctx.trait_env,
        type_id,
        trait_name,
        method_name,
        &mut ctx.tt.borrow_mut(),
    )?;
    Some(MethodCallInfo {
        local_name: local_name.clone(),
        monomorph_info: Some(monomorph_info),
        impl_module: blanket_module,
        template: Some(template),
    })
}

/// The `LocalMethodName` a receiver of `type_id` dispatches under. A head that
/// still awaits substitution is left for monomorphization to re-derive.
fn method_name_for_type(
    type_id: TypeId,
    trait_name: &FqTraitName,
    method_name: &str,
    tt: &Rc<RefCell<TypeTable>>,
) -> LocalMethodName {
    let tt_ref = tt.borrow();
    // Generic containers dispatch by (base name, struct type args) via
    // `generic_dispatch_components`. The `_` fallback below would instead mangle
    // the full `Array<i32>` / `List<i32>` spelling and trip
    // `LocalMethodName::new`'s no-`<` invariant.
    if let Some((_, type_args)) = tt_ref.generic_dispatch_components(type_id) {
        let arg_names: Vec<FqTypeName> =
            type_args.iter().map(|t| tt_ref.fq_type_name(*t)).collect();
        return LocalMethodName::new(
            tt_ref.fq_base_type_name(type_id),
            Some(trait_name.clone()),
            method_name.to_string(),
        )
        .with_struct_type_args(&arg_names);
    }
    let resolved = tt_ref.get(type_id).clone();
    let head = match resolved {
        ResolvedType::TypeParam { ref name, .. } | ResolvedType::TypePack { ref name, .. } => {
            FqTypeName::binder(name)
        }
        // A `fn(..)` receiver is named by the type itself, the same spelling
        // its dispatch stubs are registered under.
        ResolvedType::Function { .. } => tt_ref.fn_receiver_name(&resolved),
        // An impl written for `&X` registers under the reference.
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => tt_ref
            .fq_base_type_name(inner)
            .with_reference(RefKind::from_resolved(&resolved).expect("ref classify")),
        _ => tt_ref.fq_base_type_name(type_id),
    };
    let mut info = LocalMethodName::new(head, Some(trait_name.clone()), method_name.to_string());
    info.is_type_param_receiver = tt_ref.receiver_head_awaits_substitution(type_id);
    info
}

fn trait_impl_module(
    local_name: &LocalMethodName,
    type_id: TypeId,
    ctx: &TemplateCtx,
) -> ModuleSource {
    // The receiver's own module is the disambiguation hint: when two
    // same-named structs from different modules each auto-derive an impl
    // (e.g. `struct Widget` in module A and module B both get a
    // `Widget^Inspect`), `TraitEnv::impl_module_for` would otherwise return
    // whichever module landed first in iteration order. Passing the type's
    // module lets the lookup pick the candidate that actually corresponds
    // to this `type_id`.
    let resolved = ctx.tt.borrow().get(type_id).clone();
    let type_module = ctx.tt.borrow().nominal_head(type_id).map(|(_, m)| m);

    // Preferred path: consult the elaborator's `TraitEnv`, which knows where
    // every user-written `impl Trait for Type` block lives. This handles
    // cross-module impls like `impl Display for String` (defined in
    // `core:prelude/format`, not the module that declares `String`).
    if let Some(trait_) = local_name.trait_decl()
        && let Some(loc) = ctx.trait_env.impl_module_for(
            ImplReceiver::Of(local_name.receiver()),
            trait_,
            type_module.as_ref(),
        )
    {
        return loc.clone();
    }
    // Fallbacks for impls `TraitEnv` cannot index. An auto-derived impl lands
    // in the receiver type's module; a function type has none, so its impl is
    // derived per-module and lands under the current one after `link()`.
    if let Some(m) = type_module {
        return m;
    }
    match resolved {
        ResolvedType::Primitive(prim) => ModuleSource::of_primitive(prim),
        ResolvedType::Unit => ModuleSource::primitive(),
        ResolvedType::Function { .. } => ctx.module_src.clone(),
        _ => ModuleSource::primitive(),
    }
}
