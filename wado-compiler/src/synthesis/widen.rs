//! Expands [`TirExprKind::Widen`] once monomorphization has made every type
//! concrete. An `Option<!>` holds only `null`, so a value whose type holds one
//! is rebuilt as the wider type with `null` there and every other part moved
//! across. Each pair of types gets one helper doing that, which calls the
//! helpers of the parts that differ.

use std::cell::RefCell;
use std::rc::Rc;

use crate::compiler_item::SeqField;
use crate::flat_package::FlatPackage;
use crate::hashmap::IndexSet;
use crate::name::widen_helper_name;
use crate::tir::{
    ResolvedType, StructDef, TirBinaryOp, TirBlock, TirExpr, TirExprKind, TirFunction, TirLocal,
    TirStmt, TypeId,
};
use crate::tir_visitor::TirMutVisitor;
use crate::type_table::TypeTable;

use super::common::{
    assign, binary, block, break_stmt, builtin_call, case_chain, entry_call, expr_stmt,
    field_access, i32_const, if_stmt, let_mut_stmt, let_stmt, local_ref, loop_stmt,
    make_synthetic_free_function, mut_ref_expr, not_expr, param_local, ref_expr, return_stmt,
    struct_literal, synth_param, synth_span, unreachable_call,
};

pub fn expand_widenings(flat: &mut FlatPackage) {
    let mut widener = Widener {
        flat: &*flat,
        tt: &flat.type_table,
        named: IndexSet::default(),
        pending: Vec::new(),
    };
    flat.visit_bodies_mut(&mut widener);
    let mut helpers = Vec::new();
    while let Some((name, from, to)) = widener.pending.pop() {
        let mut helper = widener.helper(name, from, to);
        helper.module_source = flat.entry_module_source.clone();
        helpers.push(Rc::new(RefCell::new(helper)));
    }
    flat.functions.extend(helpers);
}

struct Widener<'a> {
    flat: &'a FlatPackage,
    tt: &'a RefCell<TypeTable>,
    /// The helpers called so far, by name.
    named: IndexSet<String>,
    /// The helpers called but not yet built: name, from, to.
    pending: Vec<(String, TypeId, TypeId)>,
}

impl TirMutVisitor for Widener<'_> {
    fn visit_expr(&mut self, expr: &mut TirExpr) {
        self.walk_expr(expr);
        match std::mem::replace(&mut expr.kind, TirExprKind::Unit) {
            TirExprKind::Widen { value } => *expr = self.rebuilt(*value, expr.type_id),
            kind => expr.kind = kind,
        }
    }
}

impl Widener<'_> {
    /// `value` as a `to`: itself where the types agree, else through the
    /// helper for the pair.
    fn rebuilt(&mut self, value: TirExpr, to: TypeId) -> TirExpr {
        let from = value.type_id;
        let (from_name, to_name) = {
            let tt = self.tt.borrow();
            if tt.type_key(from) == tt.type_key(to) {
                return value;
            }
            (
                tt.mangle_type_arg_erased(from),
                tt.mangle_type_arg_erased(to),
            )
        };
        let name = widen_helper_name(&from_name, &to_name);
        if self.named.insert(name.clone()) {
            self.pending.push((name.clone(), from, to));
        }
        entry_call(
            &name,
            vec![value],
            to,
            self.flat.entry_module_source.clone(),
        )
    }

    /// `fn(v: from) -> to`, rebuilding `v` part by part.
    fn helper(&mut self, name: String, from: TypeId, to: TypeId) -> TirFunction {
        let v = || local_ref(0, "v", from);
        let mut locals = vec![param_local("v", from, false)];
        let (from_head, to_head, tuples, resolved) = {
            let tt = self.tt.borrow();
            let (from_head, to_head) = (tt.representation_head(from), tt.representation_head(to));
            (
                from_head,
                to_head,
                tt.as_tuple(from_head).zip(tt.as_tuple(to_head)),
                (tt.get(from_head).clone(), tt.get(to_head).clone()),
            )
        };
        let stmts = if let Some((from_elems, to_elems)) = tuples {
            let elements = from_elems
                .iter()
                .zip(&to_elems)
                .enumerate()
                .map(|(i, (&f, &t))| {
                    self.rebuilt(
                        field_access(v(), i as u32, i.to_string(), f, synth_span()),
                        t,
                    )
                })
                .collect();
            vec![return_stmt(Some(TirExpr::new(
                TirExprKind::TupleLiteral { elements },
                to,
                synth_span(),
            )))]
        } else if let (ResolvedType::BuiltinArray(from_elem), ResolvedType::BuiltinArray(to_elem)) =
            resolved
        {
            self.array_stmts(from, to, from_elem, to_elem, &mut locals)
        } else if let Some(cases) = self.variant_cases(from_head, to_head) {
            let span = synth_span();
            let arms = cases
                .into_iter()
                .map(|(case_name, index, from_payload, to_payload)| {
                    let value = if from_payload == TypeTable::NEVER {
                        unreachable_call(to, span)
                    } else {
                        let payload = (from_payload != TypeTable::UNIT).then(|| {
                            let read = TirExpr::new(
                                TirExprKind::VariantPayload {
                                    expr: Box::new(v()),
                                    case_index: index,
                                    payload_type: from_payload,
                                },
                                from_payload,
                                span,
                            );
                            Box::new(self.rebuilt(read, to_payload))
                        });
                        TirExpr::new(
                            TirExprKind::VariantConstruct {
                                variant_type: to,
                                case_index: index,
                                case_name,
                                payload,
                            },
                            to,
                            span,
                        )
                    };
                    (index as i32, block(vec![expr_stmt(value)]))
                })
                .collect();
            let tag = || {
                TirExpr::new(
                    TirExprKind::VariantTag {
                        expr: Box::new(v()),
                    },
                    TypeTable::I32,
                    span,
                )
            };
            vec![return_stmt(Some(case_chain(tag, arms, to)))]
        } else {
            let (to_name, fields) = self.struct_fields(from_head, to_head);
            let fields: Vec<_> = fields
                .into_iter()
                .map(|(index, field_name, f, t)| {
                    let read = field_access(v(), index, field_name.clone(), f, synth_span());
                    (field_name, self.rebuilt(read, t))
                })
                .collect();
            vec![return_stmt(Some(struct_literal(to, to_name, fields)))]
        };
        make_synthetic_free_function(
            name,
            vec![synth_param("v", 0, from)],
            to,
            block(stmts),
            locals,
        )
    }

    /// A fresh `Array` of `to_elem` with each slot of `v` rebuilt into it.
    fn array_stmts(
        &mut self,
        from: TypeId,
        to: TypeId,
        from_elem: TypeId,
        to_elem: TypeId,
        locals: &mut Vec<TirLocal>,
    ) -> Vec<TirStmt> {
        let span = synth_span();
        let (from_ref, to_mut_ref) = {
            let mut tt = self.tt.borrow_mut();
            (tt.make_ref(from), tt.make_mut_ref(to))
        };
        locals.push(param_local("len", TypeTable::I32, false));
        locals.push(param_local("out", to, true));
        locals.push(param_local("index", TypeTable::I32, true));
        let v = || ref_expr(local_ref(0, "v", from), from_ref, span);
        let len = || local_ref(1, "len", TypeTable::I32);
        let out = || local_ref(2, "out", to);
        let index = || local_ref(3, "index", TypeTable::I32);
        let slot = builtin_call("array_get_value", vec![v(), index()], from_elem);
        let set = builtin_call(
            "array_set",
            vec![
                mut_ref_expr(out(), to_mut_ref, span),
                index(),
                self.rebuilt(slot, to_elem),
            ],
            TypeTable::UNIT,
        );
        let in_range = binary(TirBinaryOp::Lt, index(), len(), TypeTable::BOOL);
        let body = TirBlock::new(
            vec![
                if_stmt(not_expr(in_range, span), block(vec![break_stmt()]), None),
                expr_stmt(set),
                expr_stmt(assign(
                    index(),
                    binary(TirBinaryOp::Add, index(), i32_const(1), TypeTable::I32),
                )),
            ],
            span,
        );
        vec![
            let_stmt(
                "len",
                1,
                TypeTable::I32,
                builtin_call("array_len", vec![v()], TypeTable::I32),
            ),
            let_mut_stmt("out", 2, to, builtin_call("array_new", vec![len()], to)),
            let_mut_stmt("index", 3, TypeTable::I32, i32_const(0)),
            loop_stmt(body),
            return_stmt(Some(out())),
        ]
    }

    /// The cases of two instances of one variant: name, index, and the payload
    /// at each.
    fn variant_cases(
        &self,
        from: TypeId,
        to: TypeId,
    ) -> Option<Vec<(String, u32, TypeId, TypeId)>> {
        let mut tt = self.tt.borrow_mut();
        let def = tt.nominal_def(from)?;
        let cases = tt.variant_template_cases(def)?.to_vec();
        let args = |id| {
            tt.nominal_type_args(id)
                .expect("two instances of one variant part by their arguments")
        };
        let [from_args, to_args] = [args(from), args(to)];
        Some(
            cases
                .into_iter()
                .map(|(name, index, payload)| {
                    let f = tt.substitute_positional(payload, &from_args);
                    let t = tt.substitute_positional(payload, &to_args);
                    (name, index, f, t)
                })
                .collect(),
        )
    }

    /// The name of `to`'s struct, and the fields of two instances of one
    /// struct: index, name, and the type at each.
    fn struct_fields(
        &self,
        from: TypeId,
        to: TypeId,
    ) -> (String, Vec<(u32, String, TypeId, TypeId)>) {
        let to_name = self
            .tt
            .borrow()
            .struct_list_name(to)
            .expect("only a value holding an `Option<!>` widens");
        let fields = self
            .fields_of(from)
            .into_iter()
            .zip(self.fields_of(to))
            .map(|((index, name, f), (_, _, t))| (index, name, f, t))
            .collect();
        (to_name, fields)
    }

    /// A struct's fields as lowering writes them: a `List`'s `{repr, used}`,
    /// or the listed instance's own.
    fn fields_of(&self, id: TypeId) -> Vec<(u32, String, TypeId)> {
        let list_element = self.tt.borrow().list_element(id);
        if let Some(elem) = list_element {
            let backing = self.tt.borrow_mut().make_builtin_array(elem);
            return [
                (SeqField::Backing, backing),
                (SeqField::Len, TypeTable::I32),
            ]
            .into_iter()
            .map(|(field, ty)| (field.index(), field.field_name().to_string(), ty))
            .collect();
        }
        let tt = self.tt.borrow();
        let (def, type_args) = match tt.get(id) {
            ResolvedType::Struct { def, type_args } => (*def, type_args.as_slice()),
            ResolvedType::GenericInstance { def, type_args } => {
                (StructDef::Decl(*def), type_args.as_slice())
            }
            _ => unreachable!("only a value holding an `Option<!>` widens"),
        };
        self.flat
            .listed_struct(&tt, def, type_args)
            .expect("monomorphization lists each struct instance")
            .fields
            .iter()
            .map(|f| (f.index, f.name.clone(), f.type_id))
            .collect()
    }
}
