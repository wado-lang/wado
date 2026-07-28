//! Which parts of a CM value own linear memory, and how to give it back.
//!
//! The Canonical ABI hands a memory-backed value to the host as a pointer into
//! guest memory, and the guest allocated every buffer behind it. [`CmShape`]
//! records where those buffers are; [`synthesize_free_cm_value`] emits the
//! `realloc(ptr, size, align, 0)` calls that release them, walking nested
//! buffers the outer pointer alone cannot reach — a `list<string>` owns its
//! element array *and* one payload per element.
//!
//! Every offset, size and alignment comes from the `cm_abi` helpers the
//! lowering side uses, so the two cannot disagree about layout. [`cm_shape`]
//! panics on a type shape it does not recognise: a type reaching the CM
//! boundary without an ownership rule fails loudly on first use rather than
//! leaking silently.

use crate::ast::{NamedType, Type};
use crate::cm_abi;
use crate::component_model::{
    CmInterfaceRegistry, cm_align_with_registry_scoped, cm_size_with_registry_scoped,
};
use crate::tir::{TirBinaryOp, TirExpr, TirLocal, TirStmt, TypeTable};

use crate::synthesis::common::{
    alloc_local, assign, binary, block, break_stmt, builtin_call, expr_stmt, i32_const, if_stmt,
    let_mut_stmt, let_stmt, local_ref, loop_stmt,
};

use super::types::{CmStdlibNames, binary_add, is_unit_type};

/// Lighter than `LowerContext`: freeing reads the value out of memory rather
/// than out of a GC object, so it needs no `TypeTable`.
pub(super) struct CmShapeContext<'a> {
    pub cm_interface_registry: &'a CmInterfaceRegistry,
    pub cm_package: &'a str,
    pub names: &'a CmStdlibNames,
}

/// A part of a CM value: a record field, a variant payload, or a list element.
pub(super) struct CmField {
    pub shape: CmShape,
    /// Byte offset from the parent's address; 0 for a list element, whose
    /// address comes from the element buffer base.
    pub offset: u32,
    /// Canonical ABI size — the stride for a list element.
    pub size: u32,
    pub align: u32,
}

impl CmField {
    fn owns_memory(&self) -> bool {
        self.shape.owns_memory()
    }
}

/// How a Wado type owns linear memory once lowered to the CM ABI.
pub(super) enum CmShape {
    /// Owns nothing: integers, floats, `bool`, `char`, unit, an `enum` or
    /// `flags` discriminant, and every handle.
    ///
    /// Handles belong here because lifting *transfers* them to the host, so
    /// dropping one would be a double-drop. `Scalar` means "owns no memory",
    /// never "is four bytes".
    Scalar,
    /// `string`: `(ptr, len)` at the value's address, `len` bytes at align 1.
    Str,
    /// `list<T>`: `(ptr, len)` at the value's address, elements at
    /// `ptr + i * elem.size`.
    List(Box<CmField>),
    /// `record` or `tuple`: fields at fixed offsets, no discriminant.
    Record(Vec<CmField>),
    /// `variant`, `option` or `result`: a one-byte discriminant at offset 0
    /// selecting one payload, indexed by discriminant value.
    Variant(Vec<Option<CmField>>),
}

impl CmShape {
    pub(super) fn owns_memory(&self) -> bool {
        match self {
            CmShape::Scalar => false,
            CmShape::Str | CmShape::List(_) => true,
            CmShape::Record(fields) => fields.iter().any(CmField::owns_memory),
            CmShape::Variant(cases) => cases.iter().flatten().any(CmField::owns_memory),
        }
    }
}

/// Classify `ty` by how it owns linear memory at the CM boundary, mirroring the
/// dispatch of `lower::synthesize_lower_wasi_type_to_memory`.
pub(super) fn cm_shape(ty: &Type, ctx: &CmShapeContext<'_>) -> CmShape {
    let resolved = ctx.cm_interface_registry.resolve_type(ty);
    let names = ctx.names;
    match &resolved {
        Type::Named(named) if named.name == names.string => CmShape::Str,
        Type::Named(named) => named_shape(named, ctx),
        Type::Tuple(elems) if elems.is_empty() => CmShape::Scalar,
        Type::Tuple(elems) => CmShape::Record(field_list(elems, ctx)),
        Type::Generic(g) if g.name == names.array && g.args.len() == 1 => {
            CmShape::List(Box::new(field_of(&g.args[0], 0, ctx)))
        }
        Type::Generic(g) if g.name == names.option && g.args.len() == 1 => {
            let payload_offset = cm_abi::layout_option_with_registry_scoped(
                &g.args[0],
                ctx.cm_interface_registry,
                Some(ctx.cm_package),
            )
            .offsets[1];
            CmShape::Variant(vec![None, payload_case(&g.args[0], payload_offset, ctx)])
        }
        Type::Generic(g) if g.name == names.result && g.args.len() == 2 => {
            let payload_offset = cm_abi::layout_result_with_registry_scoped(
                &g.args[0],
                &g.args[1],
                ctx.cm_interface_registry,
                Some(ctx.cm_package),
            )
            .offsets[1];
            CmShape::Variant(vec![
                payload_case(&g.args[0], payload_offset, ctx),
                payload_case(&g.args[1], payload_offset, ctx),
            ])
        }
        Type::Generic(g) if matches!(g.name.as_str(), "Stream" | "Future" | "Own" | "Borrow") => {
            CmShape::Scalar
        }
        Type::Reference(_) | Type::MutReference(_) => CmShape::Scalar,
        other => panic!(
            "no CM memory-ownership rule for `{other:?}`; add one here alongside \
             its rule in `lower::synthesize_lower_wasi_type_to_memory`"
        ),
    }
}

/// A registry record, a registry variant, or a fixed-width leaf.
fn named_shape(named: &NamedType, ctx: &CmShapeContext<'_>) -> CmShape {
    let Some(source) = ctx
        .cm_interface_registry
        .resolve_cm_source_for(named, Some(ctx.cm_package))
    else {
        return CmShape::Scalar;
    };
    if let Some(fields) = ctx
        .cm_interface_registry
        .get_struct_fields_with_wado_names_by_source(source, &named.name)
    {
        let field_types: Vec<Type> = fields
            .iter()
            .map(|(_, _, ty)| ctx.cm_interface_registry.resolve_type(ty))
            .collect();
        return CmShape::Record(field_list(&field_types, ctx));
    }
    if let Some(cases) = ctx
        .cm_interface_registry
        .get_variant_cases_by_source(source, &named.name)
    {
        let payloads: Vec<Option<Type>> = cases.iter().map(|c| c.payload.clone()).collect();
        let payload_offset = cm_abi::variant_payload_offset_with_registry_scoped(
            payloads.iter().flatten(),
            ctx.cm_interface_registry,
            Some(ctx.cm_package),
        );
        return CmShape::Variant(
            payloads
                .iter()
                .map(|payload| {
                    payload
                        .as_ref()
                        .and_then(|ty| payload_case(ty, payload_offset, ctx))
                })
                .collect(),
        );
    }
    CmShape::Scalar
}

fn field_list(types: &[Type], ctx: &CmShapeContext<'_>) -> Vec<CmField> {
    let offsets = cm_abi::layout_fields_with_registry_scoped(
        types.iter(),
        ctx.cm_interface_registry,
        Some(ctx.cm_package),
    )
    .offsets;
    types
        .iter()
        .zip(offsets)
        .map(|(ty, offset)| field_of(ty, offset, ctx))
        .collect()
}

fn field_of(ty: &Type, offset: u32, ctx: &CmShapeContext<'_>) -> CmField {
    CmField {
        shape: cm_shape(ty, ctx),
        offset,
        size: cm_size_with_registry_scoped(ty, ctx.cm_interface_registry, Some(ctx.cm_package)),
        align: cm_align_with_registry_scoped(ty, ctx.cm_interface_registry, Some(ctx.cm_package)),
    }
}

/// A case's payload, or `None` when it is unit and so carries nothing to free.
fn payload_case(ty: &Type, offset: u32, ctx: &CmShapeContext<'_>) -> Option<CmField> {
    if is_unit_type(ty) {
        return None;
    }
    Some(field_of(ty, offset, ctx))
}

/// Free every linear-memory buffer the CM value at `addr` owns. `addr` is
/// evaluated more than once, so callers pass a local reference.
pub(super) fn synthesize_free_cm_value(
    shape: &CmShape,
    addr: &TirExpr,
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
) -> Vec<TirStmt> {
    match shape {
        CmShape::Scalar => vec![],
        CmShape::Str => vec![free_ptr_len(addr, 1, 1)],
        CmShape::List(elem) => free_list(elem, addr, next_local, locals),
        CmShape::Record(fields) => fields
            .iter()
            .filter(|f| f.owns_memory())
            .flat_map(|f| {
                synthesize_free_cm_value(&f.shape, &at_offset(addr, f.offset), next_local, locals)
            })
            .collect(),
        CmShape::Variant(cases) => free_variant(cases, addr, next_local, locals),
    }
}

/// Release the buffer behind the `(ptr, len)` pair at `addr`, shared by `string`
/// and `list`. The `len > 0` guard mirrors the lowering side, which allocates
/// nothing for an empty payload, and keeps `debug`'s poison length exact.
fn free_ptr_len(addr: &TirExpr, stride: u32, align: u32) -> TirStmt {
    let ptr = builtin_call("i32_load", vec![addr.clone()], TypeTable::I32);
    let len = builtin_call(
        "i32_load",
        vec![binary_add(addr.clone(), i32_const(4))],
        TypeTable::I32,
    );
    let size = if stride == 1 {
        len.clone()
    } else {
        binary(
            TirBinaryOp::Mul,
            len.clone(),
            i32_const(stride as i32),
            TypeTable::I32,
        )
    };
    if_stmt(
        binary(TirBinaryOp::Gt, len, i32_const(0), TypeTable::BOOL),
        block(vec![expr_stmt(builtin_call(
            "realloc",
            vec![ptr, size, i32_const(align as i32), i32_const(0)],
            TypeTable::I32,
        ))]),
        None,
    )
}

/// Walk the elements that own memory, then release the element buffer itself.
fn free_list(
    elem: &CmField,
    addr: &TirExpr,
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
) -> Vec<TirStmt> {
    let mut stmts = Vec::new();
    if elem.owns_memory() {
        let base_local = alloc_local(next_local, locals, TypeTable::I32);
        stmts.push(let_stmt(
            "__free_base",
            base_local,
            TypeTable::I32,
            builtin_call("i32_load", vec![addr.clone()], TypeTable::I32),
        ));
        let count_local = alloc_local(next_local, locals, TypeTable::I32);
        stmts.push(let_stmt(
            "__free_count",
            count_local,
            TypeTable::I32,
            builtin_call(
                "i32_load",
                vec![binary_add(addr.clone(), i32_const(4))],
                TypeTable::I32,
            ),
        ));
        let i_local = alloc_local(next_local, locals, TypeTable::I32);
        stmts.push(let_mut_stmt(
            "__free_i",
            i_local,
            TypeTable::I32,
            i32_const(0),
        ));

        let mut body = vec![if_stmt(
            binary(
                TirBinaryOp::GtEq,
                local_ref(i_local, "__free_i", TypeTable::I32),
                local_ref(count_local, "__free_count", TypeTable::I32),
                TypeTable::BOOL,
            ),
            block(vec![break_stmt()]),
            None,
        )];
        let elem_addr_local = alloc_local(next_local, locals, TypeTable::I32);
        body.push(let_stmt(
            "__free_elem_addr",
            elem_addr_local,
            TypeTable::I32,
            binary_add(
                local_ref(base_local, "__free_base", TypeTable::I32),
                binary(
                    TirBinaryOp::Mul,
                    local_ref(i_local, "__free_i", TypeTable::I32),
                    i32_const(elem.size as i32),
                    TypeTable::I32,
                ),
            ),
        ));
        body.extend(synthesize_free_cm_value(
            &elem.shape,
            &local_ref(elem_addr_local, "__free_elem_addr", TypeTable::I32),
            next_local,
            locals,
        ));
        body.push(expr_stmt(assign(
            local_ref(i_local, "__free_i", TypeTable::I32),
            binary_add(local_ref(i_local, "__free_i", TypeTable::I32), i32_const(1)),
        )));
        stmts.push(loop_stmt(block(body)));
    }
    stmts.push(free_ptr_len(addr, elem.size, elem.align));
    stmts
}

/// Load the one-byte discriminant the lowering side stored at offset 0 and free
/// the active case's payload. Cases owning no memory contribute no branch.
fn free_variant(
    cases: &[Option<CmField>],
    addr: &TirExpr,
    next_local: &mut u32,
    locals: &mut Vec<TirLocal>,
) -> Vec<TirStmt> {
    let owning: Vec<(usize, &CmField)> = cases
        .iter()
        .enumerate()
        .filter_map(|(i, case)| case.as_ref().map(|f| (i, f)))
        .filter(|(_, f)| f.owns_memory())
        .collect();
    if owning.is_empty() {
        return vec![];
    }

    let disc_local = alloc_local(next_local, locals, TypeTable::I32);
    let mut stmts = vec![let_stmt(
        "__free_disc",
        disc_local,
        TypeTable::I32,
        builtin_call("i32_load8_u", vec![addr.clone()], TypeTable::I32),
    )];
    for (index, field) in owning {
        let payload = synthesize_free_cm_value(
            &field.shape,
            &at_offset(addr, field.offset),
            next_local,
            locals,
        );
        stmts.push(if_stmt(
            binary(
                TirBinaryOp::Eq,
                local_ref(disc_local, "__free_disc", TypeTable::I32),
                i32_const(index as i32),
                TypeTable::BOOL,
            ),
            block(payload),
            None,
        ));
    }
    stmts
}

fn at_offset(addr: &TirExpr, offset: u32) -> TirExpr {
    if offset == 0 {
        addr.clone()
    } else {
        binary_add(addr.clone(), i32_const(offset as i32))
    }
}
