//! Template string expansion: `TirExprKind::TemplateString` becomes a `__tmpl`
//! labeled block of `String::with_capacity`, `push_str` calls, `Formatter`
//! construction, and `Display` / `Inspect` dispatch. Runs pre-monomorphize, so
//! the emitted trait calls resolve there — no post-mono `has_trait_impl` check
//! or standalone inspect function is needed.
//!
//! The emitted shape is a contract: `optimize::tmpl_hoist` matches it back
//! apart, which is why `optimize::const_branch_prune` leaves the block
//! un-flattened until that pass has run. Both sides name what they agree on
//! through [`crate::name`], [`CompilerItem`],
//! [`crate::compiler_item::SeqField`] and [`FormatterField`] rather than
//! spelling it out twice. `Formatter`'s sentinels are the one exception —
//! repeated in Rust, and pinned by the e2e fixture
//! `template_formatter_sentinels.wado`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use crate::compiler_item::{CompilerItem, FormatterField};
use crate::elaborator::trait_env::{BlanketBound, BlanketParamSource, ImplReceiver, TraitEnv};
use crate::format_spec::{Align, FormatKind, TemplateFormatSpec};
use crate::module_source::ModuleSource;
use crate::name::{FqTypeName, LocalMethodName, RefKind};
use crate::tir::{
    CallArg, FunctionRef, MonomorphInfo, ResolvedType, TirBlock, TirExpr, TirExprKind, TirLocal,
    TirModule, TirStmt, TirStmtKind, TirStructField, TirTemplatePart, TirUnaryOp, TypeId,
    TypeTable,
};
use crate::token::Span;

/// Every `core:prelude/format` symbol this synthesiser needs, resolved once
/// through the [`CompilerItem`] registry so a stdlib rename does not reach
/// here.
#[derive(Clone, Debug)]
pub(super) struct FormatStdlibNames {
    pub formatter: String,
    /// `formatter` prefixed by its declaring module — the form a function name
    /// embeds. The bare `formatter` stays for type-table lookups, which key a
    /// struct by its simple name plus module.
    pub formatter_fq: FqTypeName,
    /// In the order [`Align`] names them.
    pub alignment_cases: [EnumCase; 3],
    /// Keyed by kind: `#` sets a `Formatter` field, it does not pick a trait.
    traits: Vec<(FormatKind, FormatTrait)>,
}

#[derive(Clone, Debug)]
pub(super) struct FormatTrait {
    pub name: crate::name::FqTraitName,
    pub method: String,
}

#[derive(Clone, Debug)]
pub(super) struct EnumCase {
    pub name: String,
    pub index: u32,
}

impl FormatStdlibNames {
    pub fn from_type_table(type_table: &crate::tir::TypeTable) -> Self {
        let items = type_table.compiler_items();
        let case = |item| {
            let (_, _, name, index) = items.require_enum_case(item);
            EnumCase {
                name: name.to_string(),
                index,
            }
        };
        let format_trait = |item| FormatTrait {
            name: items.trait_fq(item),
            method: items.trait_method_name(item).to_string(),
        };
        let traits = vec![
            (FormatKind::Display, format_trait(CompilerItem::Display)),
            (FormatKind::Fixed, format_trait(CompilerItem::Display)),
            (FormatKind::Inspect, format_trait(CompilerItem::Inspect)),
            (FormatKind::Binary, format_trait(CompilerItem::Binary)),
            (FormatKind::Octal, format_trait(CompilerItem::Octal)),
            (FormatKind::LowerHex, format_trait(CompilerItem::LowerHex)),
            (FormatKind::UpperHex, format_trait(CompilerItem::UpperHex)),
            (FormatKind::LowerExp, format_trait(CompilerItem::LowerExp)),
            (FormatKind::UpperExp, format_trait(CompilerItem::UpperExp)),
        ];
        assert_formatter_layout(type_table);
        Self {
            formatter: items.struct_name(CompilerItem::Formatter).to_string(),
            formatter_fq: type_table.compiler_struct_fq_name(CompilerItem::Formatter),
            alignment_cases: [
                case(CompilerItem::AlignmentLeft),
                case(CompilerItem::AlignmentCenter),
                case(CompilerItem::AlignmentRight),
            ],
            traits,
        }
    }

    pub fn format_trait(&self, spec: Option<&TemplateFormatSpec>) -> &FormatTrait {
        let kind = spec.map_or(FormatKind::Display, |spec| spec.kind);
        &self
            .traits
            .iter()
            .find(|(k, _)| *k == kind)
            .expect("every format kind names a trait")
            .1
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
        .filter(|m| defs.kind(**m) == crate::defs::DefKind::Field)
        .map(|m| defs.name(*m))
        .collect();
    let expected: Vec<&str> = FormatterField::ALL.iter().map(|f| f.field_name()).collect();
    assert_eq!(
        declared, expected,
        "`Formatter`'s fields no longer match `FormatterField`"
    );
}

/// Expand all `TemplateString` nodes in a module.
///
/// Runs as part of the pre-mono synthesis phase. Template expansion emits
/// trait method calls (`Display::fmt`, `Inspect::inspect`) that the monomorphizer
/// subsequently resolves to concrete implementations.
pub fn expand_templates(
    module: &mut TirModule,
    tt: &Rc<RefCell<TypeTable>>,
    trait_env: &Arc<TraitEnv>,
) {
    let names = FormatStdlibNames::from_type_table(&tt.borrow());
    let ctx = TemplateCtx {
        tt,
        module_src: module.module_source.clone(),
        trait_env,
        names: &names,
    };
    for func_rc in &module.functions {
        let mut func = func_rc.borrow_mut();
        let local_count = func.local_count;
        if let Some(ref mut body) = func.body {
            let mut expander = TemplateExpander {
                alloc: FuncLocalAlloc {
                    next_index: local_count,
                    new_locals: Vec::new(),
                },
                ctx: &ctx,
            };
            crate::tir_visitor::TirOptVisitor::visit_block(&mut expander, body);
            func.local_count = expander.alloc.next_index;
            func.locals.extend(expander.alloc.new_locals);
        }
    }
}

/// Read-only context shared across all template-expansion helpers.
struct TemplateCtx<'a> {
    tt: &'a Rc<RefCell<TypeTable>>,
    module_src: ModuleSource,
    trait_env: &'a Arc<TraitEnv>,
    names: &'a FormatStdlibNames,
}

struct FuncLocalAlloc {
    next_index: u32,
    new_locals: Vec<TirLocal>,
}

impl FuncLocalAlloc {
    fn alloc(&mut self, type_id: TypeId) -> u32 {
        let idx = self.next_index;
        self.next_index += 1;
        self.new_locals.push(TirLocal::synth(idx, type_id, false));
        idx
    }
}

/// Rewrites every `TemplateString` in a body into its expanded block.
///
/// Traversal goes through [`TirOptVisitor`], whose walk is exhaustive over
/// `TirExprKind`, so a node added later cannot silently skip a template — which
/// reaches `lower::translate`'s `unreachable!`, not a diagnostic.
struct TemplateExpander<'a> {
    alloc: FuncLocalAlloc,
    ctx: &'a TemplateCtx<'a>,
}

impl crate::tir_visitor::TirOptVisitor for TemplateExpander<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) -> bool {
        // Closure bodies own an independent local-index namespace, so the
        // template synth locals (`__r`, `__f`, …) must be allocated there;
        // otherwise they collide with closure params or body lets and
        // `LocalCollector` merges incompatibly-typed locals into one Wasm
        // slot. Mirrors the closure-scope switch in pattern lowering.
        if let TirExprKind::Closure {
            params,
            body,
            body_locals,
            ..
        } = &mut expr.kind
        {
            let mut nested = TemplateExpander {
                alloc: FuncLocalAlloc {
                    next_index: (params.len() + body_locals.len()) as u32,
                    new_locals: Vec::new(),
                },
                ctx: self.ctx,
            };
            let changed = nested.visit_expr(body);
            // Surface the new synth locals on the closure so later passes
            // (pattern lowering, closure planning) see a `body_locals` that
            // matches the body's actual let-index range.
            body_locals.extend(nested.alloc.new_locals);
            return changed;
        }

        // Interpolations first: expanding inside out keeps a nested template
        // (`${ `${x}` }`) from being left behind in the block this one builds.
        let mut changed = crate::tir_visitor::opt_walk_expr(self, expr);

        if matches!(expr.kind, TirExprKind::TemplateString { .. }) {
            let (string_type, span) = (expr.type_id, expr.span);
            let TirExprKind::TemplateString { parts } =
                std::mem::replace(&mut expr.kind, TirExprKind::Unit)
            else {
                unreachable!("checked above")
            };
            *expr = build_template_block(parts, string_type, span, &mut self.alloc, self.ctx);
            changed = true;
        }
        changed
    }
}

/// Reserved per interpolation, on top of the literal segments' exact length.
const CAPACITY_PER_INTERPOLATION: i64 = 16;

/// Build the `__tmpl: { ... }` labeled block for a template string.
fn build_template_block(
    parts: Vec<TirTemplatePart>,
    string_type: TypeId,
    span: Span,
    alloc: &mut FuncLocalAlloc,
    ctx: &TemplateCtx,
) -> TirExpr {
    let tt = ctx.tt;
    let label = crate::name::TEMPLATE_BLOCK_LABEL.to_string();

    let capacity_estimate: i64 = parts
        .iter()
        .map(|p| match p {
            TirTemplatePart::Literal(s) => s.len() as i64,
            TirTemplatePart::Interpolation { .. } => CAPACITY_PER_INTERPOLATION,
        })
        .sum();

    let buf_index = alloc.alloc(string_type);
    let buf = BufLocal {
        index: buf_index,
        string_type,
        ref_string_type: tt.borrow_mut().make_ref(string_type),
        span,
    };

    // let mut __r = String::with_capacity(N);
    let with_capacity_call = string_call(
        CompilerItem::StringWithCapacity,
        None,
        vec![CallArg::new(int_literal(capacity_estimate, span), false)],
        string_type,
        span,
        ctx,
    );
    let mut stmts = vec![TirStmt::new(
        TirStmtKind::Let {
            name: crate::name::TEMPLATE_RESULT_LOCAL.to_string(),
            local_index: buf_index,
            is_mut: true,
            is_reactive: false,
            type_id: string_type,
            value: with_capacity_call,
            skip_value_copy: false,
        },
        span,
    )];

    let formatter_type = {
        let def = tt
            .borrow()
            .require_compiler_item_def(CompilerItem::Formatter);
        tt.borrow_mut()
            .make_struct(crate::tir::StructDef::Decl(def))
    };
    let mut_ref_formatter = tt.borrow_mut().make_mut_ref(formatter_type);
    let mut fmt_local_index: Option<u32> = None;

    for part in parts {
        match part {
            TirTemplatePart::Literal(s) => {
                let literal = TirExpr::new(TirExprKind::StringLiteral(s), string_type, span);
                stmts.push(buf.push_str(literal, ctx));
            }
            TirTemplatePart::Interpolation {
                expr: resolved,
                format_spec,
            } => {
                // Ref level is irrelevant to which trait renders the value —
                // except under `Inspect`, where `&x` renders as `&42` through
                // the ref blanket, so the dispatch type keeps its refs.
                let inner_type = strip_refs(resolved.type_id, tt);
                let kind = format_spec
                    .as_ref()
                    .map_or(FormatKind::Display, |spec| spec.kind);
                let format_trait = ctx.names.format_trait(format_spec.as_ref());

                // A plain `${s}` on a `String` is the buffer append itself.
                if inner_type == string_type && format_spec.is_none() {
                    let derefed = deref_to_inner(*resolved, string_type, span);
                    stmts.push(buf.push_str(derefed, ctx));
                    continue;
                }

                let formatter_expr = || {
                    build_formatter_expr(&buf, formatter_type, format_spec.as_ref(), tt, ctx.names)
                };
                // One `Formatter` local serves the whole block: the first
                // interpolation declares it, the rest overwrite it.
                let fmt_index = if let Some(idx) = fmt_local_index {
                    let assign = TirExpr::new(
                        TirExprKind::Assign {
                            target: Box::new(formatter_local(idx, formatter_type, span)),
                            value: Box::new(formatter_expr()),
                        },
                        TypeTable::UNIT,
                        span,
                    );
                    stmts.push(TirStmt::new(TirStmtKind::Expr(assign), span));
                    idx
                } else {
                    let idx = alloc.alloc(formatter_type);
                    fmt_local_index = Some(idx);
                    stmts.push(TirStmt::new(
                        TirStmtKind::Let {
                            name: crate::name::TEMPLATE_FORMATTER_LOCAL.to_string(),
                            local_index: idx,
                            is_mut: true,
                            is_reactive: false,
                            type_id: formatter_type,
                            value: formatter_expr(),
                            skip_value_copy: false,
                        },
                        span,
                    ));
                    idx
                };

                let fmt_mut_ref = TirExpr::new(
                    TirExprKind::Unary {
                        op: TirUnaryOp::MutRef,
                        expr: Box::new(formatter_local(fmt_index, formatter_type, span)),
                    },
                    mut_ref_formatter,
                    span,
                );
                let dispatch_type = match kind {
                    FormatKind::Inspect => resolved.type_id,
                    _ => inner_type,
                };
                stmts.extend(trait_fmt_call(
                    dispatch_type,
                    *resolved,
                    fmt_mut_ref,
                    kind,
                    format_trait,
                    span,
                    ctx,
                ));
            }
        }
    }

    // break __tmpl: __r;
    stmts.push(TirStmt::new(
        TirStmtKind::Break {
            label: Some(label.clone()),
            value: Some(buf.read()),
        },
        span,
    ));

    TirExpr::new(
        TirExprKind::LabeledBlock {
            label,
            block: TirBlock::new(stmts, span),
            result_type: string_type,
        },
        string_type,
        span,
    )
}

/// The `__r` accumulator the expanded block appends into. Every read of it —
/// the value, a `&`, a `&mut` — comes from here, so the local's identity is
/// written once.
struct BufLocal {
    index: u32,
    string_type: TypeId,
    ref_string_type: TypeId,
    span: Span,
}

impl BufLocal {
    fn read(&self) -> TirExpr {
        TirExpr::new(
            TirExprKind::Local {
                index: self.index,
                name: crate::name::TEMPLATE_RESULT_LOCAL.to_string(),
            },
            self.string_type,
            self.span,
        )
    }

    fn mut_ref(&self, tt: &Rc<RefCell<TypeTable>>) -> TirExpr {
        let mut_ref_string = tt.borrow_mut().make_mut_ref(self.string_type);
        TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::MutRef,
                expr: Box::new(self.read()),
            },
            mut_ref_string,
            self.span,
        )
    }

    /// `__r.push_str(&value)`.
    fn push_str(&self, value: TirExpr, ctx: &TemplateCtx) -> TirStmt {
        let arg = TirExpr::new(
            TirExprKind::Unary {
                op: TirUnaryOp::Ref,
                expr: Box::new(value),
            },
            self.ref_string_type,
            self.span,
        );
        let call = string_call(
            CompilerItem::StringPushStr,
            Some(self.read()),
            vec![CallArg::new(arg, false)],
            TypeTable::UNIT,
            self.span,
            ctx,
        );
        TirStmt::new(TirStmtKind::Expr(call), self.span)
    }
}

fn formatter_local(index: u32, formatter_type: TypeId, span: Span) -> TirExpr {
    TirExpr::new(
        TirExprKind::Local {
            index,
            name: crate::name::TEMPLATE_FORMATTER_LOCAL.to_string(),
        },
        formatter_type,
        span,
    )
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

/// Call an inherent `String` method named by a compiler item — `receiver`
/// present for a method call, absent for an associated function.
fn string_call(
    item: CompilerItem,
    receiver: Option<TirExpr>,
    args: Vec<CallArg>,
    return_type: TypeId,
    span: Span,
    ctx: &TemplateCtx,
) -> TirExpr {
    let (module_source, method_name) = {
        let tt = ctx.tt.borrow();
        let items = tt.compiler_items();
        let (module_source, _, method_name) = items.require_method(item);
        (module_source.clone(), method_name.to_string())
    };
    let owner = ctx
        .tt
        .borrow()
        .compiler_struct_fq_name(CompilerItem::String);
    let method_info = LocalMethodName::new(owner.clone(), None, method_name.clone());
    let func = FunctionRef {
        module_source,
        name: crate::name::MethodName::format_local(&owner, None, &method_name),
        monomorph_info: None,
        method_info: Some(method_info),
    };
    let kind = match receiver {
        Some(receiver) => TirExprKind::method_call(Box::new(receiver), func, vec![], args),
        None => TirExprKind::Call {
            func: Box::new(func),
            type_args: vec![],
            args,
            has_receiver: false,
        },
    };
    TirExpr::new(kind, return_type, span)
}

/// Build a `Formatter::new(&mut __r)` or, when the spec asks for padding or
/// precision, the full `Formatter { ... }` literal.
fn build_formatter_expr(
    buf: &BufLocal,
    formatter_type: TypeId,
    spec: Option<&TemplateFormatSpec>,
    tt: &Rc<RefCell<TypeTable>>,
    names: &FormatStdlibNames,
) -> TirExpr {
    let span = buf.span;
    let Some(spec) = spec.filter(|s| s.needs_formatter_fields()) else {
        return TirExpr::new(
            TirExprKind::Call {
                func: Box::new(FunctionRef {
                    module_source: ModuleSource::format(),
                    name: format!("{}::new", names.formatter_fq),
                    monomorph_info: None,
                    method_info: Some(LocalMethodName::new(
                        names.formatter_fq.clone(),
                        None,
                        "new".to_string(),
                    )),
                }),
                type_args: vec![],
                args: vec![CallArg::new(buf.mut_ref(tt), false)],
                has_receiver: false,
            },
            formatter_type,
            span,
        );
    };

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
                FormatterField::Buf => buf.mut_ref(tt),
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

/// Strip all `Ref` and `MutRef` wrappers from a type, returning the inner type.
fn strip_refs(type_id: TypeId, tt: &Rc<RefCell<TypeTable>>) -> TypeId {
    let mut current = type_id;
    loop {
        match tt.borrow().get(current).clone() {
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => current = inner,
            _ => return current,
        }
    }
}

/// Wrap `expr` in deref operations until its type matches `target_type`.
/// If `expr.type_id` is already `target_type`, returns `expr` unchanged.
fn deref_to_inner(expr: TirExpr, target_type: TypeId, span: Span) -> TirExpr {
    if expr.type_id == target_type {
        return expr;
    }
    // Just wrap in a single Deref — the lower phase handles multi-layer deref
    TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Deref,
            expr: Box::new(expr),
        },
        target_type,
        span,
    )
}

/// Peel a newtype receiver to its base type so a format call targets the
/// inherited impl — a newtype renders its underlying value for every format
/// kind except `Inspect` (which overrides it with the ` as Name` tag). A manual
/// `impl <Trait>` on the newtype stops the peel.
fn peel_transparent_newtype(
    type_id: TypeId,
    kind: FormatKind,
    trait_name: &crate::name::FqTraitName,
    ctx: &TemplateCtx,
) -> TypeId {
    if kind == FormatKind::Inspect {
        return type_id;
    }
    let mut tid = type_id;
    loop {
        let base = {
            let tt = ctx.tt.borrow();
            match tt.get(tid) {
                ResolvedType::Newtype { base_type, .. }
                    if ctx
                        .trait_env
                        .trait_def_of_fq(trait_name)
                        .is_none_or(|trait_| {
                            !ctx.trait_env.has_any_methodful_impl_by_receiver(
                                &tt.impl_receiver_key(tid),
                                trait_,
                            )
                        }) =>
                {
                    *base_type
                }
                _ => return tid,
            }
        };
        tid = base;
    }
}

/// Unified format trait dispatch: emit the `value.fmt(&mut f)` call,
/// delegating to the Wado-level trait implementation (including blanket impls).
fn trait_fmt_call(
    type_id: TypeId,
    val: TirExpr,
    fmt: TirExpr,
    kind: FormatKind,
    format_trait: &FormatTrait,
    span: Span,
    ctx: &TemplateCtx,
) -> Vec<TirStmt> {
    let trait_name = &format_trait.name;
    let method_name = format_trait.method.as_str();
    let target = peel_transparent_newtype(type_id, kind, trait_name, ctx);
    let (val, type_id) = if target == type_id {
        (val, type_id)
    } else {
        // Strip any refs the interpolation carried, then cast to the base — a
        // no-op on the transparent GC representation.
        let normalized = deref_to_inner(val, type_id, span);
        let cast = TirExpr::new(
            TirExprKind::Cast {
                expr: Box::new(normalized),
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
    } = method_call_info_for_type(type_id, trait_name, method_name, ctx);
    let mangled = local_name.to_mangled_name();

    let ref_type = ctx.tt.borrow_mut().make_ref(type_id);
    let receiver = TirExpr::new(
        TirExprKind::Unary {
            op: TirUnaryOp::Ref,
            expr: Box::new(val),
        },
        ref_type,
        span,
    );

    let call = TirExpr::new(
        TirExprKind::method_call(
            Box::new(receiver),
            FunctionRef {
                module_source: impl_module,
                name: mangled,
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
}

/// Build `MethodCallInfo` for a trait method call on `type_id`.
///
/// Combines name mangling, module resolution, and monomorphization metadata into
/// one place. For `Ref(T)` / `MutRef(T)`, this produces the `MonomorphInfo` needed
/// to instantiate the generic blanket impl (`impl<T: Trait> Trait for &T`) — no
/// type-specific logic is needed at the call site.
fn method_call_info_for_type(
    type_id: TypeId,
    trait_name: &crate::name::FqTraitName,
    method_name: &str,
    ctx: &TemplateCtx,
) -> MethodCallInfo {
    let tt = ctx.tt;
    let resolved = tt.borrow().get(type_id).clone();
    match resolved {
        // A `&T` / `&mut T` over a bare type parameter formats transparently as
        // the pointee (`T^Inspect`), not through the `&`-prefixing ref blanket:
        // in generic code `&T` is a borrow of a `T`, so `${v:?}` on a `&T`
        // parameter renders the `T`. A reference over a *concrete* type keeps the
        // ref blanket (`${&x:?}` → `&42`).
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner)
            if matches!(
                tt.borrow().get(inner),
                ResolvedType::TypeParam { .. } | ResolvedType::TypePack { .. }
            ) =>
        {
            let local_name = method_name_for_type(inner, trait_name, method_name, ctx.tt);
            let impl_module = trait_impl_module(&local_name, inner, ctx);
            MethodCallInfo {
                local_name,
                monomorph_info: None,
                impl_module,
            }
        }
        ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
            let ref_kind =
                RefKind::from_resolved(&tt.borrow().get(type_id).clone()).expect("ref classify");
            let inner_name = tt.borrow().fq_type_name(inner);
            let local_name = LocalMethodName::new_ref(
                ref_kind,
                Some(trait_name.clone()),
                method_name.to_string(),
            )
            .with_struct_type_args(&[inner_name]);
            let generic_name = LocalMethodName::new_ref(
                ref_kind,
                Some(trait_name.clone()),
                method_name.to_string(),
            )
            .to_mangled_name();
            MethodCallInfo {
                local_name,
                monomorph_info: Some(MonomorphInfo {
                    generic_name,
                    impl_type_args: vec![inner],
                    method_type_args: vec![],
                    is_blanket: true,
                }),
                impl_module: ModuleSource::format(),
            }
        }
        _ => {
            let local_name = method_name_for_type(type_id, trait_name, method_name, ctx.tt);
            if let Some(info) = blanket_method_call_info(&local_name, type_id, method_name, ctx) {
                return info;
            }
            let impl_module = trait_impl_module(&local_name, type_id, ctx);
            MethodCallInfo {
                local_name,
                monomorph_info: None,
                impl_module,
            }
        }
    }
}

/// The reflection kind `type_id` is, or `None` when reflection does not cover
/// it. A type is exactly one kind and its kind is fixed by its declaration, so
/// this is a structural question — it does not depend on how far the pipeline
/// has run. A `GenericInstance` takes its base declaration's kind: `Pair<i32>`
/// is a struct because `Pair` is. The sealed member handles are the one
/// declared struct reflection never covers.
fn reflect_kind_of(type_id: TypeId, tt: &TypeTable) -> Option<CompilerItem> {
    if tt
        .decl_of_type(type_id)
        .is_some_and(|decl| tt.is_sealed_reflect_member(decl))
    {
        return None;
    }
    match tt.get(type_id) {
        ResolvedType::Struct { .. } => Some(CompilerItem::ReflectStruct),
        ResolvedType::Variant { .. } => Some(CompilerItem::ReflectVariant),
        ResolvedType::Enum { .. } => Some(CompilerItem::ReflectEnum),
        ResolvedType::Flags { .. } => Some(CompilerItem::ReflectFlags),
        ResolvedType::Newtype { .. } => Some(CompilerItem::ReflectNewtype),
        ResolvedType::GenericInstance { def, .. } => {
            let name = &tt.def_name(*def).to_string();
            let module_source = &tt.def_module(*def).clone();
            // A variant is asked first: a variant declaration also registers a
            // struct-shaped payload layout under its own name, so the struct
            // lookup answers for both kinds.
            if tt.find_variant_type(*def).is_some() {
                Some(CompilerItem::ReflectVariant)
            } else if tt.find_struct_by_name(name, module_source).is_some() {
                Some(CompilerItem::ReflectStruct)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Whether `type_id` is one of the five reflection kinds, i.e. whether a
/// `Reflect*`-bounded blanket can claim it.
pub(crate) fn has_reflect_kind(type_id: TypeId, tt: &TypeTable) -> bool {
    reflect_kind_of(type_id, tt).is_some()
}

/// How deep a kind bound may look for its subject. `Own` asks the receiver
/// itself; `Inherited` also accepts what a newtype's base carries. Selection
/// tries `Own` first, so a newtype's own blanket beats one only its base
/// satisfies — WEP 2026-09-01 rank 2, at this layer.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundDepth {
    Own,
    Inherited,
}

/// The value blanket serving `receiver`, ranked: one whose bounds hold at the
/// receiver itself outranks one that holds only after peeling a newtype
/// (WEP 2026-09-01 rank 2). The single place monomorphization applies that
/// order, so its several blanket lookups cannot disagree about which one a
/// newtype takes. [`blanket_dispatch_for`] asks a narrower question and stops
/// at `Own`.
pub(crate) fn ranked_value_blanket<'a>(
    trait_env: &'a TraitEnv,
    trait_: crate::defs::DefId,
    type_module: Option<&ModuleSource>,
    receiver: TypeId,
    tt: &TypeTable,
) -> Option<&'a crate::elaborator::trait_env::BlanketImpl> {
    [BoundDepth::Own, BoundDepth::Inherited]
        .into_iter()
        .find_map(|depth| {
            trait_env.value_blanket_for_receiver(trait_, type_module, &|bounds| {
                receiver_satisfies_blanket_bounds(receiver, bounds, tt, depth)
            })
        })
}

/// The reflection trait `bound` names, or `None` for any other bound.
fn reflect_bound_item(bound: &BlanketBound, tt: &TypeTable) -> Option<CompilerItem> {
    let declared = bound.decl_ref.map(|decl| tt.defs().ast_id(decl))?;
    let items = tt.compiler_items();
    [
        CompilerItem::Reflect,
        CompilerItem::ReflectStruct,
        CompilerItem::ReflectVariant,
        CompilerItem::ReflectEnum,
        CompilerItem::ReflectFlags,
        CompilerItem::ReflectNewtype,
    ]
    .into_iter()
    .find(|item| items.trait_decl(*item) == Some(declared))
}

/// Whether a blanket derives *over reflection* — at least one of its
/// receiver-param bounds is a `Reflect*` trait. Being a claimable kind is not
/// the same question: a newtype satisfies every non-reflect bound its base
/// does, so without this an `impl<I: Iterator> IntoIterator for I` would claim
/// a newtype over a list and name a per-type impl nothing mints.
pub(crate) fn blanket_is_reflect_keyed(bounds: &[BlanketBound], tt: &TypeTable) -> bool {
    bounds
        .iter()
        .any(|bound| reflect_bound_item(bound, tt).is_some())
}

/// Whether `type_id` satisfies a blanket impl's receiver-param `bounds`. A kind
/// bound holds when the receiver is that kind, or when the base it inherits
/// from is (WEP 2026-09-01 rank 2 places the latter at depth 1, and selection
/// there decides between them); the identity root `Reflect` holds for any kind,
/// and `ReflectNewtype` for a newtype alone, since being a newtype is not
/// inherited. Any other bound is treated as satisfiable — deciding one needs
/// the elaborator's trait query, which monomorphization has no access to.
pub(crate) fn receiver_satisfies_blanket_bounds(
    type_id: TypeId,
    bounds: &[BlanketBound],
    tt: &TypeTable,
    depth: BoundDepth,
) -> bool {
    if bounds.is_empty() {
        return true;
    }
    // A value blanket's parameter is the value type, while the receiver of a
    // `&self` method — every `Serialize::serialize` call — arrives as a
    // reference. Asking the reference for its reflection kind answers `None`
    // and rejects the blanket that should have served the call.
    let receiver = tt.peel_refs(type_id);
    let kind = reflect_kind_of(receiver, tt);
    let inherited_kind = reflect_kind_of(tt.reflect_structure_head(receiver), tt);
    bounds.iter().all(|bound| {
        match reflect_bound_item(bound, tt) {
            // The root asks for a name, not for a shape: any reflected kind.
            Some(CompilerItem::Reflect) => kind.is_some(),
            // Being a newtype is the one kind fact a base cannot supply.
            Some(CompilerItem::ReflectNewtype) => kind == Some(CompilerItem::ReflectNewtype),
            Some(required) => {
                kind == Some(required)
                    || (depth == BoundDepth::Inherited && inherited_kind == Some(required))
            }
            None => true,
        }
    })
}

/// The module of a struct-like `type_id`, used as the disambiguation hint for
/// trait-impl lookups.
fn type_module_hint_tt(type_id: TypeId, tt: &TypeTable) -> Option<ModuleSource> {
    tt.nominal_head(type_id).map(|(_, m)| m)
}

/// Resolve a `type_id.trait::method()` dispatch to a blanket impl when no
/// per-type impl provides it but a blanket does and the receiver satisfies the
/// blanket's receiver-param bound. Shared by template expansion and the
/// auto-derive body synthesizer so both route blanket-derived calls (e.g. a
/// newtype's `Inspect` delegating to a `ReflectStruct`-derived base struct)
/// identically. Returns the blanket dispatch info and its home module.
pub(crate) fn blanket_dispatch_for(
    trait_env: &TraitEnv,
    type_id: TypeId,
    trait_name: &crate::name::FqTraitName,
    method_name: &str,
    tt: &mut TypeTable,
) -> Option<(MonomorphInfo, ModuleSource)> {
    let type_key = tt.impl_receiver_key(type_id);
    if trait_env
        .trait_def_of_fq(trait_name)
        .is_some_and(|trait_| trait_env.has_any_methodful_impl_by_receiver(&type_key, trait_))
    {
        return None;
    }
    let type_module = type_module_hint_tt(type_id, tt);
    // Param and pack projections must come from the same blanket, or the
    // template name would name one kind and the args another.
    // `Own` only, no peel: this asks which blanket *owns* the receiver's
    // method, and rank 2 answers at the receiver itself. Admitting the base's
    // bound here would hand a newtype over a struct to the `ReflectStruct`
    // derive, losing the ` as Name` tag its own `ReflectNewtype` derive writes.
    // The peel belongs to the pack projection, where the question is what the
    // base's structure is, not whose impl this is.
    let blanket = trait_env.value_blanket_for_receiver(
        trait_name.canonical()?,
        type_module.as_ref(),
        &|bounds| receiver_satisfies_blanket_bounds(type_id, bounds, tt, BoundDepth::Own),
    )?;
    let blanket_module = blanket.module.clone();
    let generic_name = LocalMethodName::new(
        blanket.receiver_binder(tt.defs()),
        Some(trait_name.clone()),
        method_name.to_string(),
    )
    .to_mangled_name();
    let impl_type_args = blanket_impl_args(trait_env, blanket, type_id, tt)?;
    Some((
        MonomorphInfo {
            generic_name,
            impl_type_args,
            method_type_args: vec![],
            is_blanket: true,
        },
        blanket_module,
    ))
}

/// The type args a value blanket's instance keys on for `receiver`: its
/// parameters in declaration order — the receiver at the slot the impl gave it,
/// each other projected off the receiver through the bound that names it. Every
/// projection is recorded on the receiver, since substituting a pack needs a
/// mutable table and later readers hold a shared borrow.
///
/// `None` when a projection cannot be resolved: a blanket that projects a
/// parameter the receiver cannot supply does not apply, and a partial list
/// would key the instance under an argument shape the template never declared.
pub(crate) fn blanket_impl_args(
    trait_env: &TraitEnv,
    blanket: &crate::elaborator::trait_env::BlanketImpl,
    receiver: TypeId,
    tt: &mut TypeTable,
) -> Option<Vec<TypeId>> {
    let sources = trait_env.blanket_param_sources(blanket);
    let mut args = Vec::with_capacity(sources.len());
    for source in sources {
        match source {
            BlanketParamSource::Receiver => args.push(receiver),
            BlanketParamSource::Unresolved => return None,
            BlanketParamSource::Projection(bound_trait, assoc) => {
                let projected =
                    tt.resolve_trait_assoc_type_of_instance(receiver, &bound_trait, &assoc)?;
                tt.register_assoc_type_resolution(
                    receiver,
                    crate::tir::TraitRef::bare(bound_trait),
                    assoc,
                    projected,
                );
                args.push(projected);
            }
        }
    }
    Some(args)
}

/// When no concrete or synthesized impl provides `trait_name` for `type_id` but
/// a blanket impl does (e.g. the `impl<T: ReflectStruct<FieldTypes = [..F]>, ..F: Inspect>
/// Inspect for T` struct derive), route the call through the blanket — the same
/// shape the `&T` / `&mut T` arms above build for the ref blankets.
fn blanket_method_call_info(
    local_name: &LocalMethodName,
    type_id: TypeId,
    method_name: &str,
    ctx: &TemplateCtx,
) -> Option<MethodCallInfo> {
    let trait_name = local_name.trait_name.as_ref()?;
    // A bodyless conformance marker (`impl Inspect for Point;`) registers in
    // the impl index but provides no method — under the blanket regime it means
    // "derive via the blanket", so route unless a real methodful impl exists.
    let (monomorph_info, blanket_module) = blanket_dispatch_for(
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
    })
}

/// Build a `LocalMethodName` for a concrete (post-mono) type.
///
/// Extracts the base name from the resolved type and applies type args
/// for parameterized types like `GenericInstance`, etc.
fn method_name_for_type(
    type_id: TypeId,
    trait_name: &crate::name::FqTraitName,
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
    match resolved {
        ResolvedType::TypeParam { ref name, .. } | ResolvedType::TypePack { ref name, .. } => {
            let mut info = LocalMethodName::new(
                FqTypeName::binder(name),
                Some(trait_name.clone()),
                method_name.to_string(),
            );
            info.is_type_param_receiver = true;
            info
        }
        ResolvedType::Function { .. } => {
            // A `fn(..)` receiver is named by the type itself, the same
            // spelling its dispatch stubs are registered under.
            LocalMethodName::new(
                tt_ref.fn_receiver_name(&resolved),
                Some(trait_name.clone()),
                method_name.to_string(),
            )
        }
        _ => LocalMethodName::new(
            tt_ref.fq_base_type_name(type_id),
            Some(trait_name.clone()),
            method_name.to_string(),
        ),
    }
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
    if let Some(trait_name) = local_name.base_trait_name()
        && let Some(loc) = ctx.trait_env.impl_module_for(
            ImplReceiver::Of(local_name.receiver()),
            trait_name,
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
        ResolvedType::Primitive(_) | ResolvedType::Unit => ModuleSource::primitive(),
        ResolvedType::Function { .. } => ctx.module_src.clone(),
        _ => ModuleSource::primitive(),
    }
}
