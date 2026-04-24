//! WIR-side lowering of the `builtin::copy_value::<T>` TIR intrinsic.
//!
//! Insertion itself is handled at the TIR level by `lower::value_copy`; this
//! module only keeps the helper that shapes a translated source instruction
//! into the `WirInstr::ValueCopy` deep-copy descriptor expected by codegen.
//! The helper is invoked from `wir_build::calls::translate_builtin_call`
//! when it meets `"builtin::copy_value"`.

use crate::tir::TypeId;
use crate::wir::{WirInstr, WirType};

use super::translate::FunctionTranslator;

impl FunctionTranslator<'_, '_> {
    /// Build a `ValueCopy` instruction for the given type. Uses the WIR type
    /// ID to identify the struct or variant and builds a shallow copy
    /// descriptor. Only `Option<T>` values can be null; plain struct / variant
    /// types are always non-null.
    pub(super) fn build_value_copy(&self, type_id: TypeId, expr: WirInstr) -> WirInstr {
        use crate::wir::{WirCopyField, WirCopyType};
        let wir_type = self.ctx.type_id_to_wir_type(self.type_table, type_id);
        if let WirType::Ref {
            type_id: wir_tid,
            nullable,
        } = wir_type
        {
            if self.ctx.is_variant_type(&wir_tid) {
                return WirInstr::ValueCopy {
                    type_id: wir_tid,
                    source_type: WirCopyType::Variant { cases: Vec::new() },
                    expr: Box::new(expr),
                    nullable,
                };
            }
            let field_count = self.ctx.get_struct_field_count(&wir_tid);
            let copy_fields: Vec<WirCopyField> = (0..field_count)
                .map(|i| WirCopyField {
                    index: i,
                    needs_copy: false,
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
