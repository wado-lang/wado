//! Value-copy semantics — decides when to emit defensive deep-copies for
//! composite values, and performs freshness / aliasing analysis on TIR
//! expressions so unaliased values can skip the copy.
//!
//! These methods are part of `FunctionTranslator`; see `translate.rs` for
//! the struct definition and the primary translation dispatch.

use crate::hashmap::IndexSet;
use crate::tir::{ResolvedType, TirBlock, TirExpr, TirExprKind, TirStmt, TirStmtKind, TirUnaryOp, TypeId, TypeTable};
use crate::wir::{WirInstr, WirType};

use super::translate::FunctionTranslator;

impl FunctionTranslator<'_, '_> {
    /// Check if a type requires value copy (struct, array, tuple, variant, option).
    pub(super) fn needs_value_copy(&self, type_id: TypeId) -> bool {
        match self.type_table.get(type_id) {
            ResolvedType::Struct { base_name, .. } => {
                // Box<T> types are GC reference cells for primitive boxing.
                // They should share the heap object on assignment, not deep-copy.
                // Identified by base_name set during monomorphization / boxing pass.
                base_name.as_deref() != Some("Box")
            }
            ResolvedType::GenericInstance {
                name,
                module_source,
                type_args,
            } => {
                // Box<T> types are GC reference cells for primitive boxing.
                if name == "Box" {
                    return false;
                }
                // Empty tuples (unit-like) don't need value copy.
                if TypeTable::is_tuple_type(name, module_source) && type_args.is_empty() {
                    return false;
                }
                true
            }
            ResolvedType::Variant { .. } => true,
            _ => false,
        }
    }

    /// Check if an expression is "fresh" (doesn't need value copy).
    /// Fresh values are newly created and don't alias existing data,
    /// so they can be used directly without copying.
    fn is_fresh_value(expr: &TirExpr) -> bool {
        Self::is_fresh_in_context(expr, &IndexSet::default())
    }

    /// Check if an expression is fresh, considering locals known to hold fresh values.
    ///
    /// `fresh_locals` tracks local variable indices that were assigned fresh values
    /// within the enclosing block scope. This enables copy elision for patterns like:
    /// ```text
    /// __seq_lit: {
    ///     let mut __b = Array { ... };   // __b is fresh
    ///     break __seq_lit: *__b;          // *__b is fresh (deref of fresh local)
    /// }
    /// ```
    fn is_fresh_in_context(expr: &TirExpr, fresh_locals: &IndexSet<u32>) -> bool {
        match &expr.kind {
            // Literals always produce fresh values
            TirExprKind::StringLiteral(_)
            | TirExprKind::StructLiteral { .. }
            | TirExprKind::TupleLiteral { .. }
            | TirExprKind::TupleSpread { .. }
            | TirExprKind::TupleZip { .. }
            | TirExprKind::TypePackExpansion { .. }
            | TirExprKind::Null => true,

            // All call variants return fresh values (callee constructs the return value)
            TirExprKind::Call { .. }
            | TirExprKind::MethodCall { .. }
            | TirExprKind::CmRawCall { .. }
            | TirExprKind::IndirectCall { .. } => true,

            // ClosureToCanonical creates a fresh closure struct
            TirExprKind::ClosureToCanonical { .. } => true,

            // Variant/enum constructors produce fresh values
            TirExprKind::VariantConstruct { .. } | TirExprKind::EnumConstruct { .. } => true,

            // Local variable reference: fresh if the local is known to hold a fresh value
            TirExprKind::Local { index, .. } => fresh_locals.contains(index),

            // Deref of a fresh value is still fresh (e.g., *self where self is fresh)
            TirExprKind::Unary {
                op: TirUnaryOp::Deref,
                expr: inner,
            } => Self::is_fresh_in_context(inner, fresh_locals),

            // Labeled blocks: fresh if every break value targeting this label is fresh,
            // tracking which locals hold fresh values within the block
            TirExprKind::LabeledBlock { label, block, .. } => {
                Self::block_breaks_are_fresh(label, block, fresh_locals)
            }

            // Field access on a fresh struct — the struct is unaliased,
            // so the extracted field is not shared.
            TirExprKind::FieldAccess { expr: inner, .. } => {
                Self::is_fresh_in_context(inner, fresh_locals)
            }

            // Variant payload extraction from a fresh variant — the variant
            // is unaliased, so the payload is not shared.
            TirExprKind::VariantPayload { expr: inner, .. } => {
                Self::is_fresh_in_context(inner, fresh_locals)
            }

            _ => false,
        }
    }

    /// Check if every `break label(value)` in a block has a fresh value.
    ///
    /// Tracks locals assigned fresh values within the block, enabling copy
    /// elision for inlined builder patterns where the break value references
    /// a locally-created object.
    fn block_breaks_are_fresh(label: &str, block: &TirBlock, parent_fresh: &IndexSet<u32>) -> bool {
        let mut found = false;
        let mut fresh_locals = parent_fresh.clone();
        if Self::scan_block_for_breaks(label, block, &mut found, &mut fresh_locals) {
            found
        } else {
            false
        }
    }

    /// Recursively scan a block for `Break` targeting `label`.
    /// Tracks fresh locals and returns `false` if any break value is not fresh.
    fn scan_block_for_breaks(
        label: &str,
        block: &TirBlock,
        found: &mut bool,
        fresh_locals: &mut IndexSet<u32>,
    ) -> bool {
        for stmt in &block.stmts {
            if !Self::scan_stmt_for_breaks(label, stmt, found, fresh_locals) {
                return false;
            }
        }
        true
    }

    fn scan_stmt_for_breaks(
        label: &str,
        stmt: &TirStmt,
        found: &mut bool,
        fresh_locals: &mut IndexSet<u32>,
    ) -> bool {
        match &stmt.kind {
            // Track locals assigned fresh values
            TirStmtKind::Let {
                local_index, value, ..
            } => {
                if Self::is_fresh_in_context(value, fresh_locals) {
                    fresh_locals.insert(*local_index);
                }
                true
            }
            TirStmtKind::Break {
                label: Some(l),
                value: Some(v),
            } if l == label => {
                *found = true;
                Self::is_fresh_in_context(v, fresh_locals)
            }
            TirStmtKind::If {
                then_block,
                else_block,
                ..
            } => {
                if !Self::scan_block_for_breaks(label, then_block, found, fresh_locals) {
                    return false;
                }
                if let Some(eb) = else_block
                    && !Self::scan_block_for_breaks(label, eb, found, fresh_locals)
                {
                    return false;
                }
                true
            }
            TirStmtKind::Loop { body } => {
                Self::scan_block_for_breaks(label, body, found, fresh_locals)
            }
            TirStmtKind::Expr(expr) => Self::scan_expr_for_breaks(label, expr, found, fresh_locals),
            _ => true,
        }
    }

    fn scan_expr_for_breaks(
        label: &str,
        expr: &TirExpr,
        found: &mut bool,
        fresh_locals: &mut IndexSet<u32>,
    ) -> bool {
        match &expr.kind {
            TirExprKind::LabeledBlock { block, .. } | TirExprKind::Block(block) => {
                Self::scan_block_for_breaks(label, block, found, fresh_locals)
            }
            TirExprKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                if !Self::scan_block_for_breaks(label, then_branch, found, fresh_locals) {
                    return false;
                }
                if let Some(eb) = else_branch
                    && !Self::scan_block_for_breaks(label, eb, found, fresh_locals)
                {
                    return false;
                }
                true
            }
            _ => true,
        }
    }

    /// Wrap a translated value instruction in `ValueCopy` if needed.
    pub(super) fn maybe_value_copy(&self, value: &TirExpr, translated: WirInstr) -> WirInstr {
        if self.needs_value_copy(value.type_id) && !Self::is_fresh_value(value) {
            self.build_value_copy(value.type_id, translated)
        } else {
            translated
        }
    }

    /// Copy an argument only if the corresponding callee parameter is `mut`.
    ///
    /// When a parameter is not `mut`, the callee cannot reassign it, write to its
    /// fields, or call `&mut self` methods on it — so the caller's value is safe
    /// without a defensive copy.
    pub(super) fn maybe_value_copy_if_mut(
        &self,
        value: &TirExpr,
        translated: WirInstr,
        is_mut: bool,
    ) -> WirInstr {
        if is_mut {
            self.maybe_value_copy(value, translated)
        } else {
            translated
        }
    }

    /// Check if a source expression's root local is immutable.
    /// Returns true when the expression is a Local or `FieldAccess` chain
    /// whose root local is in the immutable set. In that case, sharing
    /// the value without deep-copying is safe because neither the
    /// destination (non-mut let) nor the source can be mutated.
    pub(super) fn is_source_immutable(&self, expr: &TirExpr) -> bool {
        match &expr.kind {
            TirExprKind::Local { index, .. } => self.immutable_locals.contains(index),
            TirExprKind::FieldAccess { expr: inner, .. }
            | TirExprKind::TupleSpread { expr: inner }
            | TirExprKind::TupleZip { expr: inner }
            | TirExprKind::TypePackExpansion {
                call_expr: inner, ..
            } => self.is_source_immutable(inner),
            _ => false,
        }
    }

    /// Build a `ValueCopy` instruction for the given type.
    /// Uses the WIR type ID to identify the struct, and builds a shallow copy descriptor.
    /// Only `Option<T>` values can be null; plain struct/variant types are always non-null.
    fn build_value_copy(&self, type_id: TypeId, expr: WirInstr) -> WirInstr {
        use crate::wir::{WirCopyField, WirCopyType};
        let wir_type = self.ctx.type_id_to_wir_type(self.type_table, type_id);
        if let WirType::Ref {
            type_id: wir_tid,
            nullable,
        } = wir_type
        {
            // Variants use pass-through copy (immutable structs in the rec group)
            if self.ctx.is_variant_type(&wir_tid) {
                return WirInstr::ValueCopy {
                    type_id: wir_tid,
                    source_type: WirCopyType::Variant { cases: Vec::new() },
                    expr: Box::new(expr),
                    nullable,
                };
            }
            // Look up the WIR struct type to get field count
            let field_count = self.ctx.get_struct_field_count(&wir_tid);
            let copy_fields: Vec<WirCopyField> = (0..field_count)
                .map(|i| WirCopyField {
                    index: i,
                    needs_copy: false, // Shallow copy (field-by-field)
                    copy_type: None,
                })
                .collect();
            WirInstr::ValueCopy {
                type_id: wir_tid,
                source_type: WirCopyType::Struct {
                    fields: copy_fields,
                },
                expr: Box::new(expr),
                nullable,
            }
        } else {
            expr
        }
    }
}
