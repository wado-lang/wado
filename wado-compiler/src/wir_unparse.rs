//! WIR → pseudo-Wado unparser for `wado dump --wir --unparse`.
//!
//! Renders a `WirPackage` as readable pseudo-Wado source code for debugging.
//! The output uses Wado syntax for type definitions (struct, variant, enum)
//! and WAT-style mnemonics for arithmetic instructions (i32.add, f64.mul, etc.).

use crate::wir::{
    WirAbstractHeapType, WirArrayType, WirData, WirEnumType, WirExport, WirExportDesc, WirField,
    WirFlagsType, WirFuncType, WirFunction, WirGlobal, WirImport, WirImportDesc, WirInstr,
    WirPackage, WirStructType, WirType, WirTypeDef, WirVariantType,
};

/// Unparse a `WirPackage` into pseudo-Wado source code.
///
/// `cwd` is the current working directory used to shorten entry-point paths.
pub fn unparse_wir(module: &WirPackage, _cwd: Option<&str>) -> String {
    let mut unparser = WirUnparser::new(&module.types, &module.data);
    unparser.unparse(module);
    unparser.output
}

/// Kind of a block-like construct for branch target resolution.
#[derive(Clone, PartialEq)]
enum LabelBlockKind {
    Block,
    Loop,
    /// `if` bodies count as block depths in Wasm but are not shown with labels.
    If,
}

struct WirUnparser<'a> {
    output: String,
    indent: usize,
    /// Type definitions for struct field name lookup.
    types: &'a [WirTypeDef],
    /// Data segments for inlining `array.new_data` contents.
    data: &'a [WirData],
    /// Stack of (kind, label) for block-depth tracking and `br N` resolution.
    label_stack: Vec<(LabelBlockKind, String)>,
    /// Counter for generating unique labels.
    label_next_id: usize,
}

impl<'a> WirUnparser<'a> {
    fn new(types: &'a [WirTypeDef], data: &'a [WirData]) -> Self {
        Self {
            output: String::new(),
            indent: 0,
            types,
            data,
            label_stack: Vec::new(),
            label_next_id: 0,
        }
    }

    /// Try to inline a `array.new_data` as a readable literal.
    ///
    /// When offset and len are constant and the slice is valid, returns
    /// formatted values. For u8 arrays with valid UTF-8, returns a quoted string
    /// like `"hello"`. For other element types, returns comma-separated values.
    fn try_inline_data(
        &self,
        type_id: &crate::wir::WirTypeId,
        data_index: u32,
        offset: &WirInstr,
        len: &WirInstr,
    ) -> Option<String> {
        let WirInstr::I32Const(off) = offset else {
            return None;
        };
        let WirInstr::I32Const(length) = len else {
            return None;
        };
        let segment = self.data.get(data_index as usize)?;
        let off = (*off).cast_unsigned() as usize;
        let length = (*length).cast_unsigned() as usize;

        // Look up the element type from the array type definition
        let elem_type = {
            let idx = type_id.index() as usize;
            if let Some(WirTypeDef::Array(a)) = self.types.get(idx) {
                Some(a.element_type.clone())
            } else {
                None
            }
        };

        let byte_width = match elem_type.as_ref() {
            Some(WirType::U8 | WirType::I8 | WirType::Bool) => 1,
            Some(WirType::I16 | WirType::U16) => 2,
            Some(WirType::I32 | WirType::U32 | WirType::Char) => 4,
            Some(WirType::Enum { .. } | WirType::Flags { .. }) => 4,
            Some(WirType::I64 | WirType::U64) => 8,
            Some(WirType::F32) => 4,
            Some(WirType::F64) => 8,
            _ => 1, // fallback: treat as bytes
        };

        let byte_len = length * byte_width;
        let bytes = segment.bytes.get(off..off + byte_len)?;

        // For u8/i8: try UTF-8 string display
        if byte_width == 1 {
            if let Ok(s) = std::str::from_utf8(bytes) {
                let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
                return Some(format!("\"{escaped}\""));
            }
            let hex: Vec<String> = bytes.iter().map(|b| format!("0x{b:02x}")).collect();
            return Some(hex.join(", "));
        }

        // For multi-byte types: decode and display as values
        let mut values = Vec::with_capacity(length);
        for i in 0..length {
            let start = i * byte_width;
            let chunk = &bytes[start..start + byte_width];
            let val = match elem_type.as_ref() {
                Some(WirType::I16) => {
                    format!("{}", i16::from_le_bytes(chunk.try_into().ok()?))
                }
                Some(WirType::U16) => {
                    format!("{}", u16::from_le_bytes(chunk.try_into().ok()?))
                }
                Some(WirType::I32 | WirType::Enum { .. } | WirType::Flags { .. }) => {
                    format!("{}", i32::from_le_bytes(chunk.try_into().ok()?))
                }
                Some(WirType::U32) => {
                    format!("{}", u32::from_le_bytes(chunk.try_into().ok()?))
                }
                Some(WirType::Char) => {
                    let code = u32::from_le_bytes(chunk.try_into().ok()?);
                    if let Some(c) = char::from_u32(code) {
                        format!("'{c}'")
                    } else {
                        format!("0x{code:08x}")
                    }
                }
                Some(WirType::I64) => {
                    format!("{}_i64", i64::from_le_bytes(chunk.try_into().ok()?))
                }
                Some(WirType::U64) => {
                    format!("{}_u64", u64::from_le_bytes(chunk.try_into().ok()?))
                }
                Some(WirType::F32) => {
                    let v = f32::from_le_bytes(chunk.try_into().ok()?);
                    format!("{v}_f32")
                }
                Some(WirType::F64) => {
                    let v = f64::from_le_bytes(chunk.try_into().ok()?);
                    format!("{v}")
                }
                _ => return None,
            };
            values.push(val);
        }
        Some(values.join(", "))
    }

    /// Get the element type of a GC array type as a display string.
    fn array_elem_type_str(&self, type_id: &crate::wir::WirTypeId) -> String {
        let idx = type_id.index() as usize;
        if let Some(WirTypeDef::Array(a)) = self.types.get(idx) {
            self.fmt_type(&a.element_type)
        } else {
            // Fallback: strip "array<...>" wrapper from the display name.
            let tid = type_id.to_string();
            if let Some(inner) = tid.strip_prefix("array<").and_then(|s| s.strip_suffix('>')) {
                inner.to_string()
            } else {
                tid
            }
        }
    }

    /// Look up field names for a struct type by `WirTypeId`.
    fn struct_field_names(&self, type_id: &crate::wir::WirTypeId) -> Option<Vec<String>> {
        let idx = type_id.index() as usize;
        if let Some(WirTypeDef::Struct(s)) = self.types.get(idx) {
            Some(s.fields.iter().map(|f| f.name.clone()).collect())
        } else {
            None
        }
    }

    fn unparse(&mut self, module: &WirPackage) {
        // Type definitions
        for (i, type_def) in module.types.iter().enumerate() {
            self.unparse_type_def(i, type_def);
            self.newline();
        }

        // Imports
        if !module.imports.is_empty() {
            for import in &module.imports {
                self.unparse_import(import);
            }
            self.newline();
        }

        // Globals
        for global in &module.globals {
            self.unparse_global(global);
        }
        if !module.globals.is_empty() {
            self.newline();
        }

        // Functions
        for func in &module.functions {
            self.unparse_function(func);
            self.newline();
        }

        // Exports
        if !module.exports.is_empty() {
            for export in &module.exports {
                self.unparse_export(export);
            }
        }
    }

    // === Type Definitions ===

    fn unparse_type_def(&mut self, type_idx: usize, type_def: &WirTypeDef) {
        match type_def {
            WirTypeDef::Struct(s) => self.unparse_struct_type(type_idx, s),
            WirTypeDef::Variant(v) => self.unparse_variant_type(type_idx, v),
            WirTypeDef::Enum(e) => self.unparse_enum_type(type_idx, e),
            WirTypeDef::Flags(f) => self.unparse_flags_type(type_idx, f),
            WirTypeDef::Array(a) => self.unparse_array_type(type_idx, a),
            WirTypeDef::Func(f) => self.unparse_func_type(type_idx, f),
        }
    }

    fn unparse_struct_type(&mut self, type_idx: usize, s: &WirStructType) {
        self.write_indent();
        self.write("struct ");
        self.write(&s.name.fq);
        self.write(" {");

        self.unparse_type_comment(type_idx);

        if let Some(ref newtype) = s.newtype_origin {
            self.write(&format!(
                "  // newtype {} from {}",
                newtype.name, newtype.module_source
            ));
        }

        if s.fields.is_empty() {
            self.write("}");
            self.newline();
        } else {
            self.newline();
            self.indent += 1;
            for field in &s.fields {
                self.write_indent();
                self.unparse_field(field);
                self.write(",");
                self.newline();
            }
            self.indent -= 1;
            self.write_indent();
            self.write("}");
            self.newline();
        }
    }

    fn unparse_field(&mut self, field: &WirField) {
        if field.mutable {
            self.write("mut ");
        }
        self.write(&field.name);
        self.write(": ");
        self.write(&self.fmt_type(&field.ty));
    }

    fn unparse_variant_type(&mut self, type_idx: usize, v: &WirVariantType) {
        self.write_indent();
        self.write("variant ");
        self.write(&v.name.fq);
        self.write(" {");
        self.unparse_type_comment(type_idx);
        self.newline();

        self.indent += 1;
        for case in &v.cases {
            self.write_indent();
            self.write(&case.name);
            if !case.payload.is_empty() {
                self.write("(");
                for (i, ty) in case.payload.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.write(&self.fmt_type(ty));
                }
                self.write(")");
            }
            self.write(",");
            self.newline();
        }
        self.indent -= 1;

        self.write_indent();
        self.write("}");
        self.newline();
    }

    fn unparse_enum_type(&mut self, type_idx: usize, e: &WirEnumType) {
        self.write_indent();
        self.write("enum ");
        self.write(&e.name.fq);
        self.write(" { ");

        for (i, case) in e.cases.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.write(&case.name);
            self.write(" = ");
            self.write(&case.discriminant.to_string());
        }

        self.write(" };");
        self.unparse_type_comment(type_idx);
        self.newline();
    }

    fn unparse_flags_type(&mut self, type_idx: usize, f: &WirFlagsType) {
        self.write_indent();
        self.write("flags ");
        self.write(&f.name.fq);
        self.write(" { ");

        for (i, bit) in f.bits.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.write(&bit.name);
            self.write(" = ");
            self.write(&format!("bit{}", bit.position));
        }

        self.write(" };");
        self.unparse_type_comment(type_idx);
        self.newline();
    }

    fn unparse_array_type(&mut self, type_idx: usize, a: &WirArrayType) {
        self.write_indent();
        self.write("array ");
        self.write(&a.name.fq);
        if a.mutable {
            self.write(" (mut ");
        } else {
            self.write(" (");
        }
        self.write(&self.fmt_type(&a.element_type));
        self.write(");");
        self.unparse_type_comment(type_idx);
        self.newline();
    }

    fn unparse_func_type(&mut self, type_idx: usize, f: &WirFuncType) {
        self.write_indent();
        self.write("type ");
        self.write_name(&f.name.fq);
        self.write(" = fn(");
        for (i, param) in f.params.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.write(&self.fmt_type(param));
        }
        self.write(")");
        if !f.results.is_empty() {
            self.write(" -> ");
            if f.results.len() == 1 {
                self.write(&self.fmt_type(&f.results[0]));
            } else {
                self.write("[");
                for (i, r) in f.results.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.write(&self.fmt_type(r));
                }
                self.write("]");
            }
        }
        self.write(&format!(";  // TypeId({type_idx})"));
        self.newline();
    }

    // === Imports ===

    fn unparse_import(&mut self, import: &WirImport) {
        self.write_indent();
        self.write("import ");
        match &import.desc {
            WirImportDesc::Func { name, .. } => {
                self.write("fn ");
                self.write(&name.fq);
            }
            WirImportDesc::Global { ty, mutable } => {
                self.write("global ");
                if *mutable {
                    self.write("mut ");
                }
                self.write(&self.fmt_type(ty));
            }
            WirImportDesc::Memory { min, max } => {
                self.write(&format!("memory ({min}"));
                if let Some(max) = max {
                    self.write(&format!(", {max}"));
                }
                self.write(")");
            }
            WirImportDesc::Table { ty, min, max } => {
                self.write(&format!("table ({min}"));
                if let Some(max) = max {
                    self.write(&format!(", {max}"));
                }
                let ty_str = self.fmt_type(ty);
                self.write(&format!(") {ty_str}"));
            }
        }
        self.write(&format!(" from \"{}/{}\";", import.module, import.field));
        self.newline();
    }

    // === Globals ===

    fn unparse_global(&mut self, global: &WirGlobal) {
        self.write_indent();
        self.write("global ");
        if global.mutable {
            self.write("mut ");
        }
        self.write(&global.name.fq);
        self.write(": ");
        self.write(&self.fmt_type(&global.ty));
        self.write(" = ");
        self.unparse_instr_inline(&global.init);
        self.write(";");
        self.newline();
    }

    // === Functions ===

    fn unparse_function(&mut self, func: &WirFunction) {
        self.write_indent();
        self.write("fn ");
        self.write_name(&func.name.fq);
        self.write("(");

        // We need the function type to get parameter types
        // For now, just output parameter names
        for (i, name) in func.param_names.iter().enumerate() {
            if i > 0 {
                self.write(", ");
            }
            self.write(name);
        }
        self.write(")");

        if !func.effects.is_empty() {
            self.write(" with ");
            let effects_str: Vec<&str> = func
                .effects
                .iter()
                .map(super::tir::EffectRef::name)
                .collect();
            self.write(&effects_str.join(", "));
        }

        self.write(" {");
        self.newline();

        if let Some(ref body) = func.body {
            self.indent += 1;
            for instr in body {
                self.unparse_instr(instr);
            }
            self.indent -= 1;
        }

        self.write_indent();
        self.write("}");
        self.newline();
    }

    // === Instructions ===

    fn unparse_instr(&mut self, instr: &WirInstr) {
        // In statement position, expand Seq items on separate lines instead of inline.
        if let WirInstr::Seq(instrs) = instr {
            // Special case: Seq([..., val, Br]) = break-with-value.
            // Render as: "stmts...; break label val;" when val is a simple expression.
            if let Some((WirInstr::Br { depth }, rest)) = instrs.split_last()
                && let Some((val_instr, init)) = rest.split_last()
                && !is_block_expr(val_instr)
            {
                for s in init {
                    self.unparse_instr(s);
                }
                self.write_indent();
                if self.label_stack.len() > *depth as usize {
                    let (lbl, kind) = self.resolve_br(*depth);
                    let lbl = lbl.to_string();
                    let kind = kind.clone();
                    match kind {
                        LabelBlockKind::Loop => self.write(&format!("continue {lbl}: ")),
                        _ => self.write(&format!("break {lbl}: ")),
                    }
                } else {
                    self.write(&format!("br {depth}: "));
                }
                self.unparse_instr_inline(val_instr);
                self.write(";");
                self.newline();
                return;
            }
            for sub in instrs {
                self.unparse_instr(sub);
            }
            return;
        }
        // Special case: LocalSet/GlobalSet with a Seq value — expand side effects as
        // separate statements, then assign only the final value.
        if let WirInstr::LocalSet { name, value } = instr
            && let WirInstr::Seq(instrs) = value.as_ref()
            && let Some((last_val, init)) = instrs.split_last()
        {
            for s in init {
                self.unparse_instr(s);
            }
            self.write_indent();
            self.write(name);
            self.write(" = ");
            self.unparse_instr_inline(last_val);
            self.write(";");
            self.newline();
            return;
        }
        if let WirInstr::GlobalSet { name, value } = instr
            && let WirInstr::Seq(instrs) = value.as_ref()
            && let Some((last_val, init)) = instrs.split_last()
        {
            for s in init {
                self.unparse_instr(s);
            }
            self.write_indent();
            self.write(&name.fq);
            self.write(" = ");
            self.unparse_instr_inline(last_val);
            self.write(";");
            self.newline();
            return;
        }
        // Special case: Drop with a Seq operand — expand the side effects as
        // separate statements, then `drop(...)` only the final value. Without
        // this, the inline `Seq` join collapses every preamble step into one
        // unreadable line of `; `-separated instructions.
        if let WirInstr::Drop(inner) = instr
            && let WirInstr::Seq(instrs) = inner.as_ref()
            && let Some((last_val, init)) = instrs.split_last()
        {
            for s in init {
                self.unparse_instr(s);
            }
            self.write_indent();
            self.write("drop(");
            self.unparse_instr_inline(last_val);
            self.write(");");
            self.newline();
            return;
        }
        // Special case: If with a Seq condition — hoist preamble as separate statements
        // before the `if`, then render the actual condition (unwrapping bool-coercions).
        // This cleans up enum/variant pattern matching output.
        if let WirInstr::If {
            condition,
            result,
            then_body,
            else_body,
        } = instr
        {
            let (preamble, actual_cond) = self.split_condition(condition);
            let actual_cond = self.unwrap_bool_if(actual_cond);
            // Try to inline scrutinee local to eliminate the preamble entirely.
            let inlined;
            let (effective_preamble, effective_cond) =
                if let Some(inlined_cond) = Self::try_inline_scrutinee(preamble, actual_cond) {
                    inlined = inlined_cond;
                    (&[][..], &inlined as &WirInstr)
                } else {
                    (preamble, actual_cond)
                };
            if !effective_preamble.is_empty() || !std::ptr::eq(effective_cond, condition.as_ref()) {
                for s in effective_preamble {
                    self.unparse_instr(s);
                }
                let simplified = WirInstr::If {
                    condition: Box::new(effective_cond.clone()),
                    result: result.clone(),
                    then_body: then_body.clone(),
                    else_body: else_body.clone(),
                };
                self.unparse_instr(&simplified);
                return;
            }
        }

        // Render statement-level If directly to avoid the parentheses that
        // unparse_instr_inline adds around value-producing If expressions.
        if let WirInstr::If {
            condition,
            result,
            then_body,
            else_body,
        } = instr
        {
            self.write_indent();
            self.push_label(LabelBlockKind::If, None);
            self.write("if ");
            self.unparse_instr_inline(condition);
            if let Some(ty) = result {
                let ty_str = self.fmt_type(ty);
                self.write(&format!(" -> {ty_str}"));
            }
            self.write(" {");
            self.newline();
            self.indent += 1;
            for sub in then_body {
                self.unparse_instr(sub);
            }
            self.indent -= 1;
            self.unparse_else_chain(else_body.as_deref());
            self.pop_label();
            self.newline();
            return;
        }

        self.write_indent();
        self.unparse_instr_inline(instr);
        self.write(";");
        self.newline();
    }

    fn unparse_instr_inline(&mut self, instr: &WirInstr) {
        match instr {
            // Locals
            WirInstr::DeclareLocal { name, ty } => {
                let ty_str = self.fmt_type(ty);
                self.write(&format!("let {name}: {ty_str}"));
            }
            WirInstr::LocalGet { name, .. } => {
                self.write(name);
            }
            WirInstr::LocalSet { name, value } => {
                self.write(name);
                self.write(" = ");
                self.unparse_instr_inline(value);
            }
            WirInstr::LocalTee { name, value } => {
                self.write(&format!("local.tee {name}("));
                self.unparse_instr_inline(value);
                self.write(")");
            }

            // Globals
            WirInstr::GlobalGet { name, .. } => {
                self.write(&name.fq);
            }
            WirInstr::GlobalSet { name, value } => {
                self.write(&name.fq);
                self.write(" = ");
                self.unparse_instr_inline(value);
            }

            // Constants
            WirInstr::I32Const(v) => self.write(&v.to_string()),
            WirInstr::I64Const(v) => self.write(&format!("{v}_i64")),
            WirInstr::F32Const(v) => self.write(&format!("{v}_f32")),
            WirInstr::F64Const(v) => self.write(&v.to_string()),

            // i32 binary ops
            WirInstr::I32Add(a, b) => self.write_binop_op("+", a, b),
            WirInstr::I32Sub(a, b) => self.write_binop_op("-", a, b),
            WirInstr::I32Mul(a, b) => self.write_binop_op("*", a, b),
            WirInstr::I32DivS(a, b) => self.write_binop_op("/", a, b),
            WirInstr::I32DivU(a, b) => self.write_binop_op("/u", a, b),
            WirInstr::I32RemS(a, b) => self.write_binop_op("%", a, b),
            WirInstr::I32RemU(a, b) => self.write_binop_op("%u", a, b),
            WirInstr::I32And(a, b) => self.write_binop_op("&", a, b),
            WirInstr::I32Or(a, b) => self.write_binop_op("|", a, b),
            WirInstr::I32Xor(a, b) => self.write_binop_op("^", a, b),
            WirInstr::I32Shl(a, b) => self.write_binop_op("<<", a, b),
            WirInstr::I32ShrS(a, b) => self.write_binop_op(">>", a, b),
            WirInstr::I32ShrU(a, b) => self.write_binop_op(">>u", a, b),
            WirInstr::I32Eq(a, b) => self.write_binop_op("==", a, b),
            WirInstr::I32Ne(a, b) => self.write_binop_op("!=", a, b),
            WirInstr::I32LtS(a, b) => self.write_binop_op("<", a, b),
            WirInstr::I32LtU(a, b) => self.write_binop_op("<u", a, b),
            WirInstr::I32GtS(a, b) => self.write_binop_op(">", a, b),
            WirInstr::I32GtU(a, b) => self.write_binop_op(">u", a, b),
            WirInstr::I32LeS(a, b) => self.write_binop_op("<=", a, b),
            WirInstr::I32LeU(a, b) => self.write_binop_op("<=u", a, b),
            WirInstr::I32GeS(a, b) => self.write_binop_op(">=", a, b),
            WirInstr::I32GeU(a, b) => self.write_binop_op(">=u", a, b),

            // i32 unary ops
            WirInstr::I32Eqz(a) => {
                self.write_expr_with_parens(a);
                self.write(" == 0");
            }
            WirInstr::I32WrapI64(a) => self.write_unop("builtin::i32_wrap_i64", a),
            WirInstr::I32Clz(a) => self.write_unop("builtin::i32_clz", a),
            WirInstr::I32Ctz(a) => self.write_unop("builtin::i32_ctz", a),
            WirInstr::I32Popcnt(a) => self.write_unop("builtin::i32_popcnt", a),
            WirInstr::I32TruncF64S(a) => self.write_unop("builtin::i32_trunc_f64_s", a),
            WirInstr::I32TruncF64U(a) => self.write_unop("builtin::i32_trunc_f64_u", a),
            WirInstr::I32TruncF32S(a) => self.write_unop("builtin::i32_trunc_f32_s", a),
            WirInstr::I32TruncF32U(a) => self.write_unop("builtin::i32_trunc_f32_u", a),
            WirInstr::I32ReinterpretF32(a) => self.write_unop("builtin::i32_reinterpret_f32", a),
            WirInstr::I32Extend8S(a) => self.write_unop("builtin::i32_extend8_s", a),
            WirInstr::I32Extend16S(a) => self.write_unop("builtin::i32_extend16_s", a),

            // i64 binary ops
            WirInstr::I64Add(a, b) => self.write_binop_op("+", a, b),
            WirInstr::I64Sub(a, b) => self.write_binop_op("-", a, b),
            WirInstr::I64Mul(a, b) => self.write_binop_op("*", a, b),
            WirInstr::I64DivS(a, b) => self.write_binop_op("/", a, b),
            WirInstr::I64DivU(a, b) => self.write_binop_op("/u", a, b),
            WirInstr::I64RemS(a, b) => self.write_binop_op("%", a, b),
            WirInstr::I64RemU(a, b) => self.write_binop_op("%u", a, b),
            WirInstr::I64And(a, b) => self.write_binop_op("&", a, b),
            WirInstr::I64Or(a, b) => self.write_binop_op("|", a, b),
            WirInstr::I64Xor(a, b) => self.write_binop_op("^", a, b),
            WirInstr::I64Shl(a, b) => self.write_binop_op("<<", a, b),
            WirInstr::I64ShrS(a, b) => self.write_binop_op(">>", a, b),
            WirInstr::I64ShrU(a, b) => self.write_binop_op(">>u", a, b),
            WirInstr::I64Eq(a, b) => self.write_binop_op("==", a, b),
            WirInstr::I64Ne(a, b) => self.write_binop_op("!=", a, b),
            WirInstr::I64LtS(a, b) => self.write_binop_op("<", a, b),
            WirInstr::I64LtU(a, b) => self.write_binop_op("<u", a, b),
            WirInstr::I64GtS(a, b) => self.write_binop_op(">", a, b),
            WirInstr::I64GtU(a, b) => self.write_binop_op(">u", a, b),
            WirInstr::I64LeS(a, b) => self.write_binop_op("<=", a, b),
            WirInstr::I64LeU(a, b) => self.write_binop_op("<=u", a, b),
            WirInstr::I64GeS(a, b) => self.write_binop_op(">=", a, b),
            WirInstr::I64GeU(a, b) => self.write_binop_op(">=u", a, b),

            // i64 unary ops
            WirInstr::I64Eqz(a) => {
                self.write_expr_with_parens(a);
                self.write(" == 0_i64");
            }
            WirInstr::I64ExtendI32S(a) => self.write_unop("builtin::i64_extend_i32_s", a),
            WirInstr::I64ExtendI32U(a) => self.write_unop("builtin::i64_extend_i32_u", a),
            WirInstr::I64Clz(a) => self.write_unop("builtin::i64_clz", a),
            WirInstr::I64Ctz(a) => self.write_unop("builtin::i64_ctz", a),
            WirInstr::I64Popcnt(a) => self.write_unop("builtin::i64_popcnt", a),
            WirInstr::I64TruncF64S(a) => self.write_unop("builtin::i64_trunc_f64_s", a),
            WirInstr::I64TruncF64U(a) => self.write_unop("builtin::i64_trunc_f64_u", a),
            WirInstr::I64TruncF32S(a) => self.write_unop("builtin::i64_trunc_f32_s", a),
            WirInstr::I64TruncF32U(a) => self.write_unop("builtin::i64_trunc_f32_u", a),
            WirInstr::I64ReinterpretF64(a) => self.write_unop("builtin::i64_reinterpret_f64", a),

            // i128 ops
            WirInstr::I64Add128(a, b, c, d) => {
                self.write("i64.add128(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(", ");
                self.unparse_instr_inline(d);
                self.write(")");
            }
            WirInstr::I64Sub128(a, b, c, d) => {
                self.write("i64.sub128(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(", ");
                self.unparse_instr_inline(d);
                self.write(")");
            }
            WirInstr::I64MulWideU(a, b) => self.write_binop("i64.mul_wide_u", a, b),
            WirInstr::I64MulWideS(a, b) => self.write_binop("i64.mul_wide_s", a, b),

            // f32 binary ops
            WirInstr::F32Add(a, b) => self.write_binop_op("+", a, b),
            WirInstr::F32Sub(a, b) => self.write_binop_op("-", a, b),
            WirInstr::F32Mul(a, b) => self.write_binop_op("*", a, b),
            WirInstr::F32Div(a, b) => self.write_binop_op("/", a, b),
            WirInstr::F32Min(a, b) => self.write_binop("f32.min", a, b),
            WirInstr::F32Max(a, b) => self.write_binop("f32.max", a, b),
            WirInstr::F32Copysign(a, b) => self.write_binop("f32.copysign", a, b),
            WirInstr::F32Eq(a, b) => self.write_binop_op("==", a, b),
            WirInstr::F32Ne(a, b) => self.write_binop_op("!=", a, b),
            WirInstr::F32Lt(a, b) => self.write_binop_op("<", a, b),
            WirInstr::F32Gt(a, b) => self.write_binop_op(">", a, b),
            WirInstr::F32Le(a, b) => self.write_binop_op("<=", a, b),
            WirInstr::F32Ge(a, b) => self.write_binop_op(">=", a, b),

            // f32 unary ops
            WirInstr::F32Neg(a) => self.write_unop("builtin::f32_neg", a),
            WirInstr::F32Abs(a) => self.write_unop("builtin::f32_abs", a),
            WirInstr::F32Ceil(a) => self.write_unop("builtin::f32_ceil", a),
            WirInstr::F32Floor(a) => self.write_unop("builtin::f32_floor", a),
            WirInstr::F32Trunc(a) => self.write_unop("builtin::f32_trunc", a),
            WirInstr::F32Nearest(a) => self.write_unop("builtin::f32_nearest", a),
            WirInstr::F32Sqrt(a) => self.write_unop("builtin::f32_sqrt", a),
            WirInstr::F32ConvertI32S(a) => self.write_unop("builtin::f32_convert_i32_s", a),
            WirInstr::F32ConvertI32U(a) => self.write_unop("builtin::f32_convert_i32_u", a),
            WirInstr::F32ConvertI64S(a) => self.write_unop("builtin::f32_convert_i64_s", a),
            WirInstr::F32ConvertI64U(a) => self.write_unop("builtin::f32_convert_i64_u", a),
            WirInstr::F32DemoteF64(a) => self.write_unop("builtin::f32_demote_f64", a),
            WirInstr::F32ReinterpretI32(a) => self.write_unop("builtin::f32_reinterpret_i32", a),

            // f64 binary ops
            WirInstr::F64Add(a, b) => self.write_binop_op("+", a, b),
            WirInstr::F64Sub(a, b) => self.write_binop_op("-", a, b),
            WirInstr::F64Mul(a, b) => self.write_binop_op("*", a, b),
            WirInstr::F64Div(a, b) => self.write_binop_op("/", a, b),
            WirInstr::F64Min(a, b) => self.write_binop("f64.min", a, b),
            WirInstr::F64Max(a, b) => self.write_binop("f64.max", a, b),
            WirInstr::F64Copysign(a, b) => self.write_binop("f64.copysign", a, b),
            WirInstr::F64Eq(a, b) => self.write_binop_op("==", a, b),
            WirInstr::F64Ne(a, b) => self.write_binop_op("!=", a, b),
            WirInstr::F64Lt(a, b) => self.write_binop_op("<", a, b),
            WirInstr::F64Gt(a, b) => self.write_binop_op(">", a, b),
            WirInstr::F64Le(a, b) => self.write_binop_op("<=", a, b),
            WirInstr::F64Ge(a, b) => self.write_binop_op(">=", a, b),

            // f64 unary ops
            WirInstr::F64Neg(a) => self.write_unop("builtin::f64_neg", a),
            WirInstr::F64Abs(a) => self.write_unop("builtin::f64_abs", a),
            WirInstr::F64Ceil(a) => self.write_unop("builtin::f64_ceil", a),
            WirInstr::F64Floor(a) => self.write_unop("builtin::f64_floor", a),
            WirInstr::F64Trunc(a) => self.write_unop("builtin::f64_trunc", a),
            WirInstr::F64Nearest(a) => self.write_unop("builtin::f64_nearest", a),
            WirInstr::F64Sqrt(a) => self.write_unop("builtin::f64_sqrt", a),
            WirInstr::F64ConvertI32S(a) => self.write_unop("builtin::f64_convert_i32_s", a),
            WirInstr::F64ConvertI32U(a) => self.write_unop("builtin::f64_convert_i32_u", a),
            WirInstr::F64ConvertI64S(a) => self.write_unop("builtin::f64_convert_i64_s", a),
            WirInstr::F64ConvertI64U(a) => self.write_unop("builtin::f64_convert_i64_u", a),
            WirInstr::F64PromoteF32(a) => self.write_unop("builtin::f64_promote_f32", a),
            WirInstr::F64ReinterpretI64(a) => self.write_unop("builtin::f64_reinterpret_i64", a),

            // SIMD v128
            WirInstr::V128Const(v) => self.write(&format!("v128.const({v})")),
            WirInstr::V128Not(a) => self.write_unop("v128.not", a),
            WirInstr::V128And(a, b) => self.write_binop_op("v128.and", a, b),
            WirInstr::V128Or(a, b) => self.write_binop_op("v128.or", a, b),
            WirInstr::V128Xor(a, b) => self.write_binop_op("v128.xor", a, b),
            WirInstr::V128Bitselect(a, b, c) => {
                self.write("v128.bitselect(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(")");
            }
            WirInstr::I8x16Splat(a) => self.write_unop("i8x16.splat", a),
            WirInstr::I8x16ExtractLaneS(lane, a) => {
                self.write(&format!("i8x16.extract_lane_s({lane}, "));
                self.unparse_instr_inline(a);
                self.write(")");
            }
            WirInstr::I8x16ExtractLaneU(lane, a) => {
                self.write(&format!("i8x16.extract_lane_u({lane}, "));
                self.unparse_instr_inline(a);
                self.write(")");
            }
            WirInstr::I8x16ReplaceLane(lane, v, val) => {
                self.write(&format!("i8x16.replace_lane({lane}, "));
                self.unparse_instr_inline(v);
                self.write(", ");
                self.unparse_instr_inline(val);
                self.write(")");
            }
            WirInstr::I8x16Add(a, b) => self.write_binop_op("i8x16.add", a, b),
            WirInstr::I8x16Sub(a, b) => self.write_binop_op("i8x16.sub", a, b),
            WirInstr::I8x16Neg(a) => self.write_unop("i8x16.neg", a),
            WirInstr::I8x16Eq(a, b) => self.write_binop_op("i8x16.eq", a, b),
            WirInstr::I8x16Ne(a, b) => self.write_binop_op("i8x16.ne", a, b),
            WirInstr::I8x16LtS(a, b) | WirInstr::I8x16LtU(a, b) => {
                self.write_binop_op("i8x16.lt", a, b);
            }
            WirInstr::I8x16GtS(a, b) | WirInstr::I8x16GtU(a, b) => {
                self.write_binop_op("i8x16.gt", a, b);
            }
            WirInstr::I8x16LeS(a, b) | WirInstr::I8x16LeU(a, b) => {
                self.write_binop_op("i8x16.le", a, b);
            }
            WirInstr::I8x16GeS(a, b) | WirInstr::I8x16GeU(a, b) => {
                self.write_binop_op("i8x16.ge", a, b);
            }
            WirInstr::I8x16Shl(a, b) => self.write_binop_op("i8x16.shl", a, b),
            WirInstr::I8x16ShrS(a, b) => self.write_binop_op("i8x16.shr_s", a, b),
            WirInstr::I8x16ShrU(a, b) => self.write_binop_op("i8x16.shr_u", a, b),
            WirInstr::I8x16Swizzle(a, b) => self.write_binop_op("i8x16.swizzle", a, b),
            WirInstr::I8x16Shuffle(lanes, a, b) => {
                self.write(&format!("i8x16.shuffle({lanes:?}, "));
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(")");
            }
            WirInstr::I16x8Splat(a) => self.write_unop("i16x8.splat", a),
            WirInstr::I16x8ExtractLaneS(lane, a) => {
                self.write(&format!("i16x8.extract_lane_s({lane}, "));
                self.unparse_instr_inline(a);
                self.write(")");
            }
            WirInstr::I16x8ExtractLaneU(lane, a) => {
                self.write(&format!("i16x8.extract_lane_u({lane}, "));
                self.unparse_instr_inline(a);
                self.write(")");
            }
            WirInstr::I16x8ReplaceLane(lane, v, val) => {
                self.write(&format!("i16x8.replace_lane({lane}, "));
                self.unparse_instr_inline(v);
                self.write(", ");
                self.unparse_instr_inline(val);
                self.write(")");
            }
            WirInstr::I16x8Add(a, b) => self.write_binop_op("i16x8.add", a, b),
            WirInstr::I16x8Sub(a, b) => self.write_binop_op("i16x8.sub", a, b),
            WirInstr::I16x8Mul(a, b) => self.write_binop_op("i16x8.mul", a, b),
            WirInstr::I16x8Neg(a) => self.write_unop("i16x8.neg", a),
            WirInstr::I16x8Eq(a, b) => self.write_binop_op("i16x8.eq", a, b),
            WirInstr::I16x8Ne(a, b) => self.write_binop_op("i16x8.ne", a, b),
            WirInstr::I16x8LtS(a, b) | WirInstr::I16x8LtU(a, b) => {
                self.write_binop_op("i16x8.lt", a, b);
            }
            WirInstr::I16x8GtS(a, b) | WirInstr::I16x8GtU(a, b) => {
                self.write_binop_op("i16x8.gt", a, b);
            }
            WirInstr::I16x8LeS(a, b) | WirInstr::I16x8LeU(a, b) => {
                self.write_binop_op("i16x8.le", a, b);
            }
            WirInstr::I16x8GeS(a, b) | WirInstr::I16x8GeU(a, b) => {
                self.write_binop_op("i16x8.ge", a, b);
            }
            WirInstr::I16x8Shl(a, b) => self.write_binop_op("i16x8.shl", a, b),
            WirInstr::I16x8ShrS(a, b) => self.write_binop_op("i16x8.shr_s", a, b),
            WirInstr::I16x8ShrU(a, b) => self.write_binop_op("i16x8.shr_u", a, b),
            WirInstr::I32x4Splat(a) => self.write_unop("i32x4.splat", a),
            WirInstr::I32x4ExtractLane(lane, a) => {
                self.write(&format!("i32x4.extract_lane({lane}, "));
                self.unparse_instr_inline(a);
                self.write(")");
            }
            WirInstr::I32x4ReplaceLane(lane, v, val) => {
                self.write(&format!("i32x4.replace_lane({lane}, "));
                self.unparse_instr_inline(v);
                self.write(", ");
                self.unparse_instr_inline(val);
                self.write(")");
            }
            WirInstr::I32x4Add(a, b) => self.write_binop_op("+", a, b),
            WirInstr::I32x4Sub(a, b) => self.write_binop_op("-", a, b),
            WirInstr::I32x4Mul(a, b) => self.write_binop_op("*", a, b),
            WirInstr::I32x4Neg(a) => self.write_unop("i32x4.neg", a),
            WirInstr::I32x4Eq(a, b) => self.write_binop_op("i32x4.eq", a, b),
            WirInstr::I32x4Ne(a, b) => self.write_binop_op("i32x4.ne", a, b),
            WirInstr::I32x4LtS(a, b) | WirInstr::I32x4LtU(a, b) => {
                self.write_binop_op("i32x4.lt", a, b);
            }
            WirInstr::I32x4GtS(a, b) | WirInstr::I32x4GtU(a, b) => {
                self.write_binop_op("i32x4.gt", a, b);
            }
            WirInstr::I32x4LeS(a, b) | WirInstr::I32x4LeU(a, b) => {
                self.write_binop_op("i32x4.le", a, b);
            }
            WirInstr::I32x4GeS(a, b) | WirInstr::I32x4GeU(a, b) => {
                self.write_binop_op("i32x4.ge", a, b);
            }
            WirInstr::I32x4Shl(a, b) => self.write_binop_op("i32x4.shl", a, b),
            WirInstr::I32x4ShrS(a, b) => self.write_binop_op("i32x4.shr_s", a, b),
            WirInstr::I32x4ShrU(a, b) => self.write_binop_op("i32x4.shr_u", a, b),
            WirInstr::I64x2Splat(a) => self.write_unop("i64x2.splat", a),
            WirInstr::I64x2ExtractLane(lane, a) => {
                self.write(&format!("i64x2.extract_lane({lane}, "));
                self.unparse_instr_inline(a);
                self.write(")");
            }
            WirInstr::I64x2ReplaceLane(lane, v, val) => {
                self.write(&format!("i64x2.replace_lane({lane}, "));
                self.unparse_instr_inline(v);
                self.write(", ");
                self.unparse_instr_inline(val);
                self.write(")");
            }
            WirInstr::I64x2Add(a, b) => self.write_binop_op("+", a, b),
            WirInstr::I64x2Sub(a, b) => self.write_binop_op("-", a, b),
            WirInstr::I64x2Mul(a, b) => self.write_binop_op("*", a, b),
            WirInstr::I64x2Neg(a) => self.write_unop("i64x2.neg", a),
            WirInstr::I64x2Eq(a, b) => self.write_binop_op("i64x2.eq", a, b),
            WirInstr::I64x2Ne(a, b) => self.write_binop_op("i64x2.ne", a, b),
            WirInstr::I64x2LtS(a, b) => self.write_binop_op("i64x2.lt_s", a, b),
            WirInstr::I64x2GtS(a, b) => self.write_binop_op("i64x2.gt_s", a, b),
            WirInstr::I64x2LeS(a, b) => self.write_binop_op("i64x2.le_s", a, b),
            WirInstr::I64x2GeS(a, b) => self.write_binop_op("i64x2.ge_s", a, b),
            WirInstr::I64x2Shl(a, b) => self.write_binop_op("i64x2.shl", a, b),
            WirInstr::I64x2ShrS(a, b) => self.write_binop_op("i64x2.shr_s", a, b),
            WirInstr::I64x2ShrU(a, b) => self.write_binop_op("i64x2.shr_u", a, b),
            WirInstr::F32x4Splat(a) => self.write_unop("f32x4.splat", a),
            WirInstr::F32x4ExtractLane(lane, a) => {
                self.write(&format!("f32x4.extract_lane({lane}, "));
                self.unparse_instr_inline(a);
                self.write(")");
            }
            WirInstr::F32x4ReplaceLane(lane, v, val) => {
                self.write(&format!("f32x4.replace_lane({lane}, "));
                self.unparse_instr_inline(v);
                self.write(", ");
                self.unparse_instr_inline(val);
                self.write(")");
            }
            WirInstr::F32x4Add(a, b) => self.write_binop_op("+", a, b),
            WirInstr::F32x4Sub(a, b) => self.write_binop_op("-", a, b),
            WirInstr::F32x4Mul(a, b) => self.write_binop_op("*", a, b),
            WirInstr::F32x4Div(a, b) => self.write_binop_op("/", a, b),
            WirInstr::F32x4Neg(a) => self.write_unop("f32x4.neg", a),
            WirInstr::F32x4Sqrt(a) => self.write_unop("f32x4.sqrt", a),
            WirInstr::F32x4Abs(a) => self.write_unop("f32x4.abs", a),
            WirInstr::F32x4Eq(a, b) => self.write_binop_op("f32x4.eq", a, b),
            WirInstr::F32x4Ne(a, b) => self.write_binop_op("f32x4.ne", a, b),
            WirInstr::F32x4Lt(a, b) => self.write_binop_op("f32x4.lt", a, b),
            WirInstr::F32x4Gt(a, b) => self.write_binop_op("f32x4.gt", a, b),
            WirInstr::F32x4Le(a, b) => self.write_binop_op("f32x4.le", a, b),
            WirInstr::F32x4Ge(a, b) => self.write_binop_op("f32x4.ge", a, b),
            WirInstr::F32x4Min(a, b) => self.write_binop_op("f32x4.min", a, b),
            WirInstr::F32x4Max(a, b) => self.write_binop_op("f32x4.max", a, b),
            WirInstr::F64x2Splat(a) => self.write_unop("f64x2.splat", a),
            WirInstr::F64x2ExtractLane(lane, a) => {
                self.write(&format!("f64x2.extract_lane({lane}, "));
                self.unparse_instr_inline(a);
                self.write(")");
            }
            WirInstr::F64x2ReplaceLane(lane, v, val) => {
                self.write(&format!("f64x2.replace_lane({lane}, "));
                self.unparse_instr_inline(v);
                self.write(", ");
                self.unparse_instr_inline(val);
                self.write(")");
            }
            WirInstr::F64x2Add(a, b) => self.write_binop_op("+", a, b),
            WirInstr::F64x2Sub(a, b) => self.write_binop_op("-", a, b),
            WirInstr::F64x2Mul(a, b) => self.write_binop_op("*", a, b),
            WirInstr::F64x2Div(a, b) => self.write_binop_op("/", a, b),
            WirInstr::F64x2Neg(a) => self.write_unop("f64x2.neg", a),
            WirInstr::F64x2Sqrt(a) => self.write_unop("f64x2.sqrt", a),
            WirInstr::F64x2Abs(a) => self.write_unop("f64x2.abs", a),
            WirInstr::F64x2Eq(a, b) => self.write_binop_op("f64x2.eq", a, b),
            WirInstr::F64x2Ne(a, b) => self.write_binop_op("f64x2.ne", a, b),
            WirInstr::F64x2Lt(a, b) => self.write_binop_op("f64x2.lt", a, b),
            WirInstr::F64x2Gt(a, b) => self.write_binop_op("f64x2.gt", a, b),
            WirInstr::F64x2Le(a, b) => self.write_binop_op("f64x2.le", a, b),
            WirInstr::F64x2Ge(a, b) => self.write_binop_op("f64x2.ge", a, b),
            WirInstr::F64x2Min(a, b) => self.write_binop_op("f64x2.min", a, b),
            WirInstr::F64x2Max(a, b) => self.write_binop_op("f64x2.max", a, b),
            WirInstr::I8x16Abs(a) => self.write_unop("i8x16.abs", a),
            WirInstr::I8x16AddSatS(a, b) => self.write_binop_op("i8x16.add_sat_s", a, b),
            WirInstr::I8x16AddSatU(a, b) => self.write_binop_op("i8x16.add_sat_u", a, b),
            WirInstr::I8x16SubSatS(a, b) => self.write_binop_op("i8x16.sub_sat_s", a, b),
            WirInstr::I8x16SubSatU(a, b) => self.write_binop_op("i8x16.sub_sat_u", a, b),
            WirInstr::I8x16MinS(a, b) => self.write_binop_op("i8x16.min_s", a, b),
            WirInstr::I8x16MinU(a, b) => self.write_binop_op("i8x16.min_u", a, b),
            WirInstr::I8x16MaxS(a, b) => self.write_binop_op("i8x16.max_s", a, b),
            WirInstr::I8x16MaxU(a, b) => self.write_binop_op("i8x16.max_u", a, b),
            WirInstr::I8x16AvgrU(a, b) => self.write_binop_op("i8x16.avgr_u", a, b),
            WirInstr::I8x16AllTrue(a) => self.write_unop("i8x16.all_true", a),
            WirInstr::I8x16Bitmask(a) => self.write_unop("i8x16.bitmask", a),
            WirInstr::I8x16NarrowI16x8S(a, b) => self.write_binop_op("i8x16.narrow_i16x8_s", a, b),
            WirInstr::I8x16NarrowI16x8U(a, b) => self.write_binop_op("i8x16.narrow_i16x8_u", a, b),
            WirInstr::I8x16Popcnt(a) => self.write_unop("i8x16.popcnt", a),
            WirInstr::I16x8Abs(a) => self.write_unop("i16x8.abs", a),
            WirInstr::I16x8AddSatS(a, b) => self.write_binop_op("i16x8.add_sat_s", a, b),
            WirInstr::I16x8AddSatU(a, b) => self.write_binop_op("i16x8.add_sat_u", a, b),
            WirInstr::I16x8SubSatS(a, b) => self.write_binop_op("i16x8.sub_sat_s", a, b),
            WirInstr::I16x8SubSatU(a, b) => self.write_binop_op("i16x8.sub_sat_u", a, b),
            WirInstr::I16x8MinS(a, b) => self.write_binop_op("i16x8.min_s", a, b),
            WirInstr::I16x8MinU(a, b) => self.write_binop_op("i16x8.min_u", a, b),
            WirInstr::I16x8MaxS(a, b) => self.write_binop_op("i16x8.max_s", a, b),
            WirInstr::I16x8MaxU(a, b) => self.write_binop_op("i16x8.max_u", a, b),
            WirInstr::I16x8AvgrU(a, b) => self.write_binop_op("i16x8.avgr_u", a, b),
            WirInstr::I16x8AllTrue(a) => self.write_unop("i16x8.all_true", a),
            WirInstr::I16x8Bitmask(a) => self.write_unop("i16x8.bitmask", a),
            WirInstr::I16x8NarrowI32x4S(a, b) => self.write_binop_op("i16x8.narrow_i32x4_s", a, b),
            WirInstr::I16x8NarrowI32x4U(a, b) => self.write_binop_op("i16x8.narrow_i32x4_u", a, b),
            WirInstr::I16x8ExtendLowI8x16S(a) => self.write_unop("i16x8.extend_low_i8x16_s", a),
            WirInstr::I16x8ExtendHighI8x16S(a) => self.write_unop("i16x8.extend_high_i8x16_s", a),
            WirInstr::I16x8ExtendLowI8x16U(a) => self.write_unop("i16x8.extend_low_i8x16_u", a),
            WirInstr::I16x8ExtendHighI8x16U(a) => self.write_unop("i16x8.extend_high_i8x16_u", a),
            WirInstr::I16x8ExtMulLowI8x16S(a, b) => {
                self.write_binop_op("i16x8.extmul_low_i8x16_s", a, b);
            }
            WirInstr::I16x8ExtMulHighI8x16S(a, b) => {
                self.write_binop_op("i16x8.extmul_high_i8x16_s", a, b);
            }
            WirInstr::I16x8ExtMulLowI8x16U(a, b) => {
                self.write_binop_op("i16x8.extmul_low_i8x16_u", a, b);
            }
            WirInstr::I16x8ExtMulHighI8x16U(a, b) => {
                self.write_binop_op("i16x8.extmul_high_i8x16_u", a, b);
            }
            WirInstr::I16x8ExtAddPairwiseI8x16S(a) => {
                self.write_unop("i16x8.extadd_pairwise_i8x16_s", a);
            }
            WirInstr::I16x8ExtAddPairwiseI8x16U(a) => {
                self.write_unop("i16x8.extadd_pairwise_i8x16_u", a);
            }
            WirInstr::I16x8Q15MulrSatS(a, b) => self.write_binop_op("i16x8.q15mulr_sat_s", a, b),
            WirInstr::I32x4Abs(a) => self.write_unop("i32x4.abs", a),
            WirInstr::I32x4AllTrue(a) => self.write_unop("i32x4.all_true", a),
            WirInstr::I32x4Bitmask(a) => self.write_unop("i32x4.bitmask", a),
            WirInstr::I32x4MinS(a, b) => self.write_binop_op("i32x4.min_s", a, b),
            WirInstr::I32x4MinU(a, b) => self.write_binop_op("i32x4.min_u", a, b),
            WirInstr::I32x4MaxS(a, b) => self.write_binop_op("i32x4.max_s", a, b),
            WirInstr::I32x4MaxU(a, b) => self.write_binop_op("i32x4.max_u", a, b),
            WirInstr::I32x4DotI16x8S(a, b) => self.write_binop_op("i32x4.dot_i16x8_s", a, b),
            WirInstr::I32x4ExtendLowI16x8S(a) => self.write_unop("i32x4.extend_low_i16x8_s", a),
            WirInstr::I32x4ExtendHighI16x8S(a) => self.write_unop("i32x4.extend_high_i16x8_s", a),
            WirInstr::I32x4ExtendLowI16x8U(a) => self.write_unop("i32x4.extend_low_i16x8_u", a),
            WirInstr::I32x4ExtendHighI16x8U(a) => self.write_unop("i32x4.extend_high_i16x8_u", a),
            WirInstr::I32x4ExtMulLowI16x8S(a, b) => {
                self.write_binop_op("i32x4.extmul_low_i16x8_s", a, b);
            }
            WirInstr::I32x4ExtMulHighI16x8S(a, b) => {
                self.write_binop_op("i32x4.extmul_high_i16x8_s", a, b);
            }
            WirInstr::I32x4ExtMulLowI16x8U(a, b) => {
                self.write_binop_op("i32x4.extmul_low_i16x8_u", a, b);
            }
            WirInstr::I32x4ExtMulHighI16x8U(a, b) => {
                self.write_binop_op("i32x4.extmul_high_i16x8_u", a, b);
            }
            WirInstr::I32x4ExtAddPairwiseI16x8S(a) => {
                self.write_unop("i32x4.extadd_pairwise_i16x8_s", a);
            }
            WirInstr::I32x4ExtAddPairwiseI16x8U(a) => {
                self.write_unop("i32x4.extadd_pairwise_i16x8_u", a);
            }
            WirInstr::I32x4TruncSatF32x4S(a) => self.write_unop("i32x4.trunc_sat_f32x4_s", a),
            WirInstr::I32x4TruncSatF32x4U(a) => self.write_unop("i32x4.trunc_sat_f32x4_u", a),
            WirInstr::I32x4TruncSatF64x2SZero(a) => {
                self.write_unop("i32x4.trunc_sat_f64x2_s_zero", a);
            }
            WirInstr::I32x4TruncSatF64x2UZero(a) => {
                self.write_unop("i32x4.trunc_sat_f64x2_u_zero", a);
            }
            WirInstr::I64x2Abs(a) => self.write_unop("i64x2.abs", a),
            WirInstr::I64x2AllTrue(a) => self.write_unop("i64x2.all_true", a),
            WirInstr::I64x2Bitmask(a) => self.write_unop("i64x2.bitmask", a),
            WirInstr::I64x2ExtendLowI32x4S(a) => self.write_unop("i64x2.extend_low_i32x4_s", a),
            WirInstr::I64x2ExtendHighI32x4S(a) => self.write_unop("i64x2.extend_high_i32x4_s", a),
            WirInstr::I64x2ExtendLowI32x4U(a) => self.write_unop("i64x2.extend_low_i32x4_u", a),
            WirInstr::I64x2ExtendHighI32x4U(a) => self.write_unop("i64x2.extend_high_i32x4_u", a),
            WirInstr::I64x2ExtMulLowI32x4S(a, b) => {
                self.write_binop_op("i64x2.extmul_low_i32x4_s", a, b);
            }
            WirInstr::I64x2ExtMulHighI32x4S(a, b) => {
                self.write_binop_op("i64x2.extmul_high_i32x4_s", a, b);
            }
            WirInstr::I64x2ExtMulLowI32x4U(a, b) => {
                self.write_binop_op("i64x2.extmul_low_i32x4_u", a, b);
            }
            WirInstr::I64x2ExtMulHighI32x4U(a, b) => {
                self.write_binop_op("i64x2.extmul_high_i32x4_u", a, b);
            }
            WirInstr::F32x4Ceil(a) => self.write_unop("f32x4.ceil", a),
            WirInstr::F32x4Floor(a) => self.write_unop("f32x4.floor", a),
            WirInstr::F32x4Trunc(a) => self.write_unop("f32x4.trunc", a),
            WirInstr::F32x4Nearest(a) => self.write_unop("f32x4.nearest", a),
            WirInstr::F32x4PMin(a, b) => self.write_binop_op("f32x4.pmin", a, b),
            WirInstr::F32x4PMax(a, b) => self.write_binop_op("f32x4.pmax", a, b),
            WirInstr::F32x4ConvertI32x4S(a) => self.write_unop("f32x4.convert_i32x4_s", a),
            WirInstr::F32x4ConvertI32x4U(a) => self.write_unop("f32x4.convert_i32x4_u", a),
            WirInstr::F32x4DemoteF64x2Zero(a) => self.write_unop("f32x4.demote_f64x2_zero", a),
            WirInstr::F64x2Ceil(a) => self.write_unop("f64x2.ceil", a),
            WirInstr::F64x2Floor(a) => self.write_unop("f64x2.floor", a),
            WirInstr::F64x2Trunc(a) => self.write_unop("f64x2.trunc", a),
            WirInstr::F64x2Nearest(a) => self.write_unop("f64x2.nearest", a),
            WirInstr::F64x2PMin(a, b) => self.write_binop_op("f64x2.pmin", a, b),
            WirInstr::F64x2PMax(a, b) => self.write_binop_op("f64x2.pmax", a, b),
            WirInstr::F64x2ConvertLowI32x4S(a) => self.write_unop("f64x2.convert_low_i32x4_s", a),
            WirInstr::F64x2ConvertLowI32x4U(a) => self.write_unop("f64x2.convert_low_i32x4_u", a),
            WirInstr::F64x2PromoteLowF32x4(a) => self.write_unop("f64x2.promote_low_f32x4", a),
            WirInstr::V128AndNot(a, b) => self.write_binop_op("v128.andnot", a, b),
            WirInstr::V128AnyTrue(a) => self.write_unop("v128.any_true", a),
            WirInstr::I8x16RelaxedSwizzle(a, b) => {
                self.write_binop_op("i8x16.relaxed_swizzle", a, b);
            }
            WirInstr::I8x16RelaxedLaneselect(a, b, c) => {
                self.write("i8x16.relaxed_laneselect(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(")");
            }
            WirInstr::I16x8RelaxedLaneselect(a, b, c) => {
                self.write("i16x8.relaxed_laneselect(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(")");
            }
            WirInstr::I32x4RelaxedLaneselect(a, b, c) => {
                self.write("i32x4.relaxed_laneselect(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(")");
            }
            WirInstr::I64x2RelaxedLaneselect(a, b, c) => {
                self.write("i64x2.relaxed_laneselect(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(")");
            }
            WirInstr::F32x4RelaxedMadd(a, b, c) => {
                self.write("f32x4.relaxed_madd(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(")");
            }
            WirInstr::F32x4RelaxedNmadd(a, b, c) => {
                self.write("f32x4.relaxed_nmadd(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(")");
            }
            WirInstr::F64x2RelaxedMadd(a, b, c) => {
                self.write("f64x2.relaxed_madd(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(")");
            }
            WirInstr::F64x2RelaxedNmadd(a, b, c) => {
                self.write("f64x2.relaxed_nmadd(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(")");
            }
            WirInstr::F32x4RelaxedMin(a, b) => {
                self.write_binop_op("f32x4.relaxed_min", a, b);
            }
            WirInstr::F32x4RelaxedMax(a, b) => {
                self.write_binop_op("f32x4.relaxed_max", a, b);
            }
            WirInstr::F64x2RelaxedMin(a, b) => {
                self.write_binop_op("f64x2.relaxed_min", a, b);
            }
            WirInstr::F64x2RelaxedMax(a, b) => {
                self.write_binop_op("f64x2.relaxed_max", a, b);
            }
            WirInstr::I32x4RelaxedTruncF32x4S(a) => {
                self.write_unop("i32x4.relaxed_trunc_f32x4_s", a);
            }
            WirInstr::I32x4RelaxedTruncF32x4U(a) => {
                self.write_unop("i32x4.relaxed_trunc_f32x4_u", a);
            }
            WirInstr::I32x4RelaxedTruncF64x2SZero(a) => {
                self.write_unop("i32x4.relaxed_trunc_f64x2_s_zero", a);
            }
            WirInstr::I32x4RelaxedTruncF64x2UZero(a) => {
                self.write_unop("i32x4.relaxed_trunc_f64x2_u_zero", a);
            }
            WirInstr::I16x8RelaxedQ15mulrS(a, b) => {
                self.write_binop_op("i16x8.relaxed_q15mulr_s", a, b);
            }
            WirInstr::I16x8RelaxedDotI8x16I7x16S(a, b) => {
                self.write_binop_op("i16x8.relaxed_dot_i8x16_i7x16_s", a, b);
            }
            WirInstr::I32x4RelaxedDotI8x16I7x16AddS(a, b, c) => {
                self.write("i32x4.relaxed_dot_i8x16_i7x16_add_s(");
                self.unparse_instr_inline(a);
                self.write(", ");
                self.unparse_instr_inline(b);
                self.write(", ");
                self.unparse_instr_inline(c);
                self.write(")");
            }

            // GC: Struct
            WirInstr::StructNew { type_id, fields } => {
                let tid = type_id.to_string();
                let field_names = self.struct_field_names(type_id);
                self.write("struct.new ");
                self.write_name(&tid);
                self.write(" { ");
                for (i, f) in fields.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    if let Some(ref names) = field_names
                        && let Some(name) = names.get(i)
                    {
                        self.write(name);
                        self.write(": ");
                    }
                    self.unparse_instr_inline(f);
                }
                self.write(" }");
            }
            WirInstr::StructGet {
                field_name, expr, ..
            } => {
                self.unparse_instr_inline(expr);
                self.write(".");
                self.write(field_name);
            }
            WirInstr::StructSet {
                field_name,
                expr,
                value,
                ..
            } => {
                self.unparse_instr_inline(expr);
                self.write(".");
                self.write(field_name);
                self.write(" = ");
                self.unparse_instr_inline(value);
            }

            // GC: Array
            WirInstr::ArrayNew { type_id, init, len } => {
                let elem = self.array_elem_type_str(type_id);
                self.write(&format!("array.new<{elem}>("));
                self.unparse_instr_inline(init);
                self.write(", ");
                self.unparse_instr_inline(len);
                self.write(")");
            }
            WirInstr::ArrayNewDefault { type_id, len } => {
                let elem = self.array_elem_type_str(type_id);
                self.write(&format!("builtin::array_new<{elem}>("));
                self.unparse_instr_inline(len);
                self.write(")");
            }
            WirInstr::ArrayNewData {
                type_id,
                data_index,
                offset,
                len,
            } => {
                let elem = self.array_elem_type_str(type_id);
                // Try to inline the data segment contents when offset and len are constants
                if let Some(inline) = self.try_inline_data(type_id, *data_index, offset, len) {
                    self.write(&format!("array.new_data<{elem}>({inline})"));
                } else {
                    self.write(&format!("array.new_data<{elem}>[{data_index}]("));
                    self.unparse_instr_inline(offset);
                    self.write(", ");
                    self.unparse_instr_inline(len);
                    self.write(")");
                }
            }
            WirInstr::ArrayNewFixed { type_id, elements } => {
                let elem = self.array_elem_type_str(type_id);
                self.write(&format!("array.new_fixed<{elem}>("));
                for (i, e) in elements.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.unparse_instr_inline(e);
                }
                self.write(")");
            }
            WirInstr::ArrayGet {
                type_id,
                array,
                index,
                ..
            } => {
                let elem = self.array_elem_type_str(type_id);
                self.write(&format!("builtin::array_get<{elem}>("));
                self.unparse_instr_inline(array);
                self.write(", ");
                self.unparse_instr_inline(index);
                self.write(")");
            }
            WirInstr::ArrayGetS {
                type_id,
                array,
                index,
                ..
            } => {
                let elem = self.array_elem_type_str(type_id);
                self.write(&format!("array.get_s<{elem}>("));
                self.unparse_instr_inline(array);
                self.write(", ");
                self.unparse_instr_inline(index);
                self.write(")");
            }
            WirInstr::ArrayGetU {
                type_id,
                array,
                index,
                ..
            } => {
                let elem = self.array_elem_type_str(type_id);
                let fname = if elem == "u8" {
                    "builtin::array_get_u8".to_string()
                } else {
                    format!("array.get_u<{elem}>")
                };
                self.write(&format!("{fname}("));
                self.unparse_instr_inline(array);
                self.write(", ");
                self.unparse_instr_inline(index);
                self.write(")");
            }
            WirInstr::ArraySet {
                type_id,
                array,
                index,
                value,
            } => {
                let elem = self.array_elem_type_str(type_id);
                let fname = if elem == "u8" {
                    "builtin::array_set_u8".to_string()
                } else {
                    format!("builtin::array_set<{elem}>")
                };
                self.write(&format!("{fname}("));
                self.unparse_instr_inline(array);
                self.write(", ");
                self.unparse_instr_inline(index);
                self.write(", ");
                self.unparse_instr_inline(value);
                self.write(")");
            }
            WirInstr::ArrayLen(a) => {
                self.write("builtin::array_len(");
                self.unparse_instr_inline(a);
                self.write(")");
            }
            WirInstr::ArrayCopy {
                dest_type_id,
                src_type_id: _,
                dest,
                dest_offset,
                src,
                src_offset,
                len,
            } => {
                let elem = self.array_elem_type_str(dest_type_id);
                self.write(&format!("builtin::array_copy<{elem}>("));
                self.unparse_instr_inline(dest);
                self.write(", ");
                self.unparse_instr_inline(dest_offset);
                self.write(", ");
                self.unparse_instr_inline(src);
                self.write(", ");
                self.unparse_instr_inline(src_offset);
                self.write(", ");
                self.unparse_instr_inline(len);
                self.write(")");
            }
            WirInstr::ArrayFill {
                type_id,
                array,
                offset,
                value,
                len,
            } => {
                let elem = self.array_elem_type_str(type_id);
                self.write(&format!("builtin::array_fill<{elem}>("));
                self.unparse_instr_inline(array);
                self.write(", ");
                self.unparse_instr_inline(offset);
                self.write(", ");
                self.unparse_instr_inline(value);
                self.write(", ");
                self.unparse_instr_inline(len);
                self.write(")");
            }
            WirInstr::ArrayClone {
                type_id,
                src,
                element_copy_func,
            } => {
                let elem = self.array_elem_type_str(type_id);
                if let Some(func) = element_copy_func {
                    self.write(&format!(
                        "builtin::array_clone_deep<{elem}>(/*via {func}*/ "
                    ));
                } else {
                    self.write(&format!("builtin::array_clone<{elem}>("));
                }
                self.unparse_instr_inline(src);
                self.write(")");
            }

            // GC: Reference
            WirInstr::RefNull { heap_type } => {
                self.write(&format!(
                    "ref.null {}",
                    format_abstract_heap_type(heap_type)
                ));
            }
            WirInstr::RefIsNull(a) => self.write_unop("ref.is_null", a),
            WirInstr::RefAsNonNull(a) => self.write_unop("ref.as_non_null", a),
            WirInstr::RefCast {
                type_id,
                nullable,
                expr,
            } => {
                let null_str = if *nullable { " null" } else { "" };
                let tid = type_id.to_string();
                self.write(&format!("ref.cast{null_str} "));
                self.write_name(&tid);
                self.write("(");
                self.unparse_instr_inline(expr);
                self.write(")");
            }
            WirInstr::RefTest {
                type_id,
                nullable,
                expr,
            } => {
                let null_str = if *nullable { " null" } else { "" };
                let tid = type_id.to_string();
                self.write(&format!("ref.test{null_str} "));
                self.write_name(&tid);
                self.write("(");
                self.unparse_instr_inline(expr);
                self.write(")");
            }
            WirInstr::RefEq(a, b) => self.write_binop("ref.eq", a, b),
            WirInstr::RefI31(a) => self.write_unop("ref.i31", a),
            WirInstr::I31GetS(a) => self.write_unop("i31.get_s", a),
            WirInstr::I31GetU(a) => self.write_unop("i31.get_u", a),
            WirInstr::ExternInternalize(a) => self.write_unop("extern.internalize", a),
            WirInstr::ExternExternalize(a) => self.write_unop("extern.externalize", a),

            // Control Flow
            WirInstr::Block {
                label,
                result,
                body,
            } => {
                let targeted = is_block_targeted_from(body, 0);
                let lbl = self.push_label(LabelBlockKind::Block, label.as_deref());
                if targeted {
                    self.write(&format!("{lbl}: "));
                }
                self.write("block");
                if let Some(ty) = result {
                    let ty_str = self.fmt_type(ty);
                    self.write(&format!(" -> {ty_str}"));
                }
                self.write(" {");
                self.newline();
                self.indent += 1;
                for instr in body {
                    self.unparse_instr(instr);
                }
                self.indent -= 1;
                self.write_indent();
                self.write("}");
                self.pop_label();
            }
            WirInstr::Loop { label, body } => {
                let targeted = is_block_targeted_from(body, 0);
                let lbl = self.push_label(LabelBlockKind::Loop, label.as_deref());
                if targeted {
                    self.write(&format!("{lbl}: "));
                }
                self.write("loop {");
                self.newline();
                self.indent += 1;
                for instr in body {
                    self.unparse_instr(instr);
                }
                self.indent -= 1;
                self.write_indent();
                self.write("}");
                self.pop_label();
            }
            WirInstr::If {
                condition,
                result,
                then_body,
                else_body,
            } => {
                // Wrap value-producing If expressions (those with a result type) in
                // parentheses so they don't produce ambiguous `if if ...` when nested
                // inside another condition or expression.
                let wrap = result.is_some();
                if wrap {
                    self.write("(");
                }
                // Push If onto the label stack so `br N` inside can correctly
                // count depths, but don't show a label since `if` is not a
                // labeled construct in pseudo-Wado.
                self.push_label(LabelBlockKind::If, None);
                self.write("if ");
                self.unparse_instr_inline(condition);
                if let Some(ty) = result {
                    let ty_str = self.fmt_type(ty);
                    self.write(&format!(" -> {ty_str}"));
                }
                self.write(" {");
                self.newline();
                self.indent += 1;
                for instr in then_body {
                    self.unparse_instr(instr);
                }
                self.indent -= 1;
                self.unparse_else_chain(else_body.as_deref());
                self.pop_label();
                if wrap {
                    self.write(")");
                }
            }
            WirInstr::BranchHint { likely, expr } => {
                let hint = if *likely { "likely" } else { "unlikely" };
                self.write(&format!("builtin::{hint}("));
                self.unparse_instr_inline(expr);
                self.write(")");
            }
            WirInstr::Br { depth } => {
                if self.label_stack.len() > *depth as usize {
                    let (lbl, kind) = self.resolve_br(*depth);
                    let lbl = lbl.to_string();
                    let kind = kind.clone();
                    match kind {
                        LabelBlockKind::Loop => self.write(&format!("continue {lbl}")),
                        _ => self.write(&format!("break {lbl}")),
                    }
                } else {
                    self.write(&format!("br {depth}"));
                }
            }
            WirInstr::BrIf { depth, condition } => {
                if self.label_stack.len() > *depth as usize {
                    let (lbl, kind) = self.resolve_br(*depth);
                    let lbl = lbl.to_string();
                    let kind = kind.clone();
                    let kw = match kind {
                        LabelBlockKind::Loop => "continue_if",
                        _ => "break_if",
                    };
                    self.write(&format!("{kw} {lbl} "));
                    self.unparse_instr_inline(condition);
                } else {
                    self.write(&format!("br_if {depth} "));
                    self.unparse_instr_inline(condition);
                }
            }
            WirInstr::BrTable {
                index,
                targets,
                default,
            } => {
                self.write("br_table ");
                self.unparse_instr_inline(index);
                self.write(" [");
                for (i, t) in targets.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.write(&t.to_string());
                }
                self.write(&format!("] default={default}"));
            }
            WirInstr::Return { value } => {
                self.write("return");
                if let Some(v) = value {
                    self.write(" ");
                    // Multi-value return: render comma-separated instead of
                    // Seq's default semicolon separator.
                    if let WirInstr::Seq(instrs) = v.as_ref() {
                        for (i, instr) in instrs.iter().enumerate() {
                            if i > 0 {
                                self.write(", ");
                            }
                            self.unparse_instr_inline(instr);
                        }
                    } else {
                        self.unparse_instr_inline(v);
                    }
                }
            }
            WirInstr::Unreachable => self.write("unreachable"),
            WirInstr::Nop => self.write("nop"),
            WirInstr::Drop(a) => self.write_unop("drop", a),
            WirInstr::Select {
                condition,
                if_true,
                if_false,
                ty,
            } => {
                self.write("select");
                if let Some(ty) = ty {
                    let ty_str = self.fmt_type(ty);
                    self.write(&format!(" {ty_str}"));
                }
                self.write("(");
                self.unparse_instr_inline(condition);
                self.write(", ");
                self.unparse_instr_inline(if_true);
                self.write(", ");
                self.unparse_instr_inline(if_false);
                self.write(")");
            }

            // Calls
            WirInstr::Call { func_id, args } => {
                let fid = func_id.to_string();
                self.write_name(&fid);
                self.write("(");
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        self.write(", ");
                    }
                    self.unparse_instr_inline(a);
                }
                self.write(")");
            }
            WirInstr::CallIndirect {
                type_id,
                table,
                index,
                args,
            } => {
                let tid = type_id.to_string();
                self.write("call_indirect ");
                self.write_name(&tid);
                self.write(&format!(" table={table}("));
                self.unparse_instr_inline(index);
                for a in args {
                    self.write(", ");
                    self.unparse_instr_inline(a);
                }
                self.write(")");
            }
            WirInstr::CallRef {
                type_id,
                func_ref,
                args,
            } => {
                let tid = type_id.to_string();
                self.write("call_ref ");
                self.write_name(&tid);
                self.write("(");
                self.unparse_instr_inline(func_ref);
                for a in args {
                    self.write(", ");
                    self.unparse_instr_inline(a);
                }
                self.write(")");
            }
            WirInstr::RefFunc { func_id } => {
                let fid = func_id.to_string();
                self.write("ref.func ");
                self.write_name(&fid);
            }

            // Memory
            WirInstr::MemorySize => self.write("builtin::memory_size()"),
            WirInstr::MemoryGrow(a) => self.write_unop("builtin::memory_grow", a),
            WirInstr::MemoryFill { dst, value, len } => {
                self.write("builtin::memory_fill(");
                self.unparse_instr_inline(dst);
                self.write(", ");
                self.unparse_instr_inline(value);
                self.write(", ");
                self.unparse_instr_inline(len);
                self.write(")");
            }
            WirInstr::I32Load { offset, addr, .. } => {
                self.write_mem_load("builtin::load_i32", *offset, addr);
            }
            WirInstr::I32Load8U { offset, addr, .. } => {
                self.write_mem_load("builtin::load_u8", *offset, addr);
            }
            WirInstr::I32Load8S { offset, addr, .. } => {
                self.write_mem_load("builtin::load_i8", *offset, addr);
            }
            WirInstr::I32Load16U { offset, addr, .. } => {
                self.write_mem_load("builtin::load_u16", *offset, addr);
            }
            WirInstr::I32Load16S { offset, addr, .. } => {
                self.write_mem_load("builtin::load_i16", *offset, addr);
            }
            WirInstr::I32Store {
                offset,
                addr,
                value,
                ..
            } => {
                self.write_mem_store("builtin::store_i32", *offset, addr, value);
            }
            WirInstr::I32Store8 {
                offset,
                addr,
                value,
                ..
            } => {
                self.write_mem_store("builtin::store_u8", *offset, addr, value);
            }
            WirInstr::I32Store16 {
                offset,
                addr,
                value,
                ..
            } => {
                self.write_mem_store("builtin::store_u16", *offset, addr, value);
            }
            WirInstr::I64Load { offset, addr, .. } => {
                self.write_mem_load("builtin::load_i64", *offset, addr);
            }
            WirInstr::I64Store {
                offset,
                addr,
                value,
                ..
            } => {
                self.write_mem_store("builtin::store_i64", *offset, addr, value);
            }
            WirInstr::V128Load { offset, addr, .. } => {
                self.write_mem_load("builtin::load_v128", *offset, addr);
            }
            WirInstr::V128Store {
                offset,
                addr,
                value,
                ..
            } => {
                self.write_mem_store("builtin::store_v128", *offset, addr, value);
            }

            // Table
            WirInstr::TableGet { table, index } => {
                self.write(&format!("table.get {table}("));
                self.unparse_instr_inline(index);
                self.write(")");
            }
            WirInstr::TableSet {
                table,
                index,
                value,
            } => {
                self.write(&format!("table.set {table}("));
                self.unparse_instr_inline(index);
                self.write(", ");
                self.unparse_instr_inline(value);
                self.write(")");
            }

            WirInstr::MultiValueStructNew { type_id, instr } => {
                let tid = type_id.to_string();
                self.write("multivalue_struct_new ");
                self.write_name(&tid);
                self.write("(");
                self.unparse_instr_inline(instr);
                self.write(")");
            }
            WirInstr::MultiValueLocalBind { instr, locals } => {
                let names: Vec<_> = locals.iter().map(|l| l.as_deref().unwrap_or("_")).collect();
                self.write(&format!("multivalue_bind [{}] = ", names.join(", ")));
                self.unparse_instr_inline(instr);
            }

            WirInstr::Seq(instrs) => {
                for (i, instr) in instrs.iter().enumerate() {
                    if i > 0 {
                        self.write("; ");
                    }
                    self.unparse_instr_inline(instr);
                }
            }
        }
    }

    // === Exports ===

    fn unparse_export(&mut self, export: &WirExport) {
        self.write_indent();
        self.write("export ");
        match &export.desc {
            WirExportDesc::Func { func_id } => {
                let fid = func_id.to_string();
                self.write("fn ");
                self.write_name(&fid);
            }
            WirExportDesc::Global { name } => {
                self.write(&format!("global {}", name.fq));
            }
            WirExportDesc::Memory => {
                self.write("memory");
            }
            WirExportDesc::Table { index } => {
                self.write(&format!("table {index}"));
            }
        }
        self.write(&format!(" as \"{}\"", export.name));
        self.newline();
    }

    // === Helpers ===

    fn write_binop(&mut self, name: &str, a: &WirInstr, b: &WirInstr) {
        self.write(name);
        self.write("(");
        self.unparse_instr_inline(a);
        self.write(", ");
        self.unparse_instr_inline(b);
        self.write(")");
    }

    /// Write `a op b` using infix operator syntax.
    fn write_binop_op(&mut self, op: &str, a: &WirInstr, b: &WirInstr) {
        self.write_expr_with_parens(a);
        self.write(&format!(" {op} "));
        self.write_expr_with_parens(b);
    }

    /// Write `instr`, wrapping in parens if it renders as an infix operator expression.
    fn write_expr_with_parens(&mut self, instr: &WirInstr) {
        if is_op_instr(instr) {
            self.write("(");
            self.unparse_instr_inline(instr);
            self.write(")");
        } else {
            self.unparse_instr_inline(instr);
        }
    }

    fn write_unop(&mut self, name: &str, a: &WirInstr) {
        self.write(name);
        self.write("(");
        self.unparse_instr_inline(a);
        self.write(")");
    }

    fn write_mem_load(&mut self, name: &str, offset: u64, addr: &WirInstr) {
        self.write(name);
        if offset != 0 {
            self.write(&format!("[{offset}]"));
        }
        self.write("(");
        self.unparse_instr_inline(addr);
        self.write(")");
    }

    fn write_mem_store(&mut self, name: &str, offset: u64, addr: &WirInstr, value: &WirInstr) {
        self.write(name);
        if offset != 0 {
            self.write(&format!("[{offset}]"));
        }
        self.write("(");
        self.unparse_instr_inline(addr);
        self.write(", ");
        self.unparse_instr_inline(value);
        self.write(")");
    }

    /// Print the else-chain of an `if`, flattening `} else { if ... }` into `} else if ...`.
    ///
    /// The caller must have already pushed a label for the outer If onto `label_stack`.
    /// Each flattened `else if` pushes/pops its own If label.
    fn unparse_else_chain(&mut self, else_body: Option<&[WirInstr]>) {
        if let Some(else_body) = else_body {
            // Flatten single-if else into `else if` (regardless of result type).
            if else_body.len() == 1
                && let WirInstr::If {
                    condition,
                    result,
                    then_body,
                    else_body: inner_else,
                } = &else_body[0]
            {
                // Each else-if branch is a new If in Wasm depth terms.
                // Unwrap bool-coercion and inline scrutinee locals to get a clean condition.
                let (preamble, actual_cond) = self.split_condition(condition);
                let actual_cond = self.unwrap_bool_if(actual_cond);
                let inlined;
                let effective_cond =
                    if let Some(inlined_cond) = Self::try_inline_scrutinee(preamble, actual_cond) {
                        inlined = inlined_cond;
                        &inlined as &WirInstr
                    } else {
                        // Fall back: hoist preamble before the else-if
                        for s in preamble {
                            self.unparse_instr(s);
                        }
                        actual_cond
                    };
                self.push_label(LabelBlockKind::If, None);
                self.write_indent();
                self.write("} else if ");
                self.unparse_instr_inline(effective_cond);
                if let Some(ty) = result {
                    let ty_str = self.fmt_type(ty);
                    self.write(&format!(" -> {ty_str}"));
                }
                self.write(" {");
                self.newline();
                self.indent += 1;
                for instr in then_body {
                    self.unparse_instr(instr);
                }
                self.indent -= 1;
                self.unparse_else_chain(inner_else.as_deref());
                self.pop_label();
                return;
            }
            // Regular else
            self.write_indent();
            self.write("} else {");
            self.newline();
            self.indent += 1;
            for instr in else_body {
                self.unparse_instr(instr);
            }
            self.indent -= 1;
        }
        // Close the if/else block
        self.write_indent();
        self.write("}");
    }

    fn unparse_type_comment(&mut self, type_idx: usize) {
        self.write(&format!("  // TypeId({type_idx})"));
    }

    /// Format a `WirType` with shortened names.
    fn fmt_type(&self, ty: &WirType) -> String {
        match ty {
            WirType::I8 => "i8".to_string(),
            WirType::I16 => "i16".to_string(),
            WirType::I32 => "i32".to_string(),
            WirType::I64 => "i64".to_string(),
            WirType::U8 => "u8".to_string(),
            WirType::U16 => "u16".to_string(),
            WirType::U32 => "u32".to_string(),
            WirType::U64 => "u64".to_string(),
            WirType::F32 => "f32".to_string(),
            WirType::F64 => "f64".to_string(),
            WirType::V128 => "v128".to_string(),
            WirType::Bool => "bool".to_string(),
            WirType::Char => "char".to_string(),
            WirType::Unit => "()".to_string(),
            WirType::Enum { type_id } => quote_if_needed(&type_id.to_string()),
            WirType::Flags { type_id } => quote_if_needed(&type_id.to_string()),
            WirType::Ref { type_id, nullable } => {
                let short = quote_if_needed(&type_id.to_string());
                if *nullable {
                    format!("ref null {short}")
                } else {
                    format!("ref {short}")
                }
            }
            WirType::AbstractRef {
                heap_type,
                nullable,
            } => {
                let ht = format_abstract_heap_type(heap_type);
                if *nullable {
                    format!("ref null {ht}")
                } else {
                    format!("ref {ht}")
                }
            }
        }
    }

    /// Push a block/loop/if onto the label stack, returning the assigned label.
    fn push_label(&mut self, kind: LabelBlockKind, existing: Option<&str>) -> String {
        let label = existing.map(String::from).unwrap_or_else(|| {
            let prefix = match kind {
                LabelBlockKind::Block => "b",
                LabelBlockKind::Loop => "l",
                LabelBlockKind::If => "_i",
            };
            let id = self.label_next_id;
            self.label_next_id += 1;
            format!("{prefix}{id}")
        });
        self.label_stack.push((kind, label.clone()));
        label
    }

    fn pop_label(&mut self) {
        self.label_stack.pop();
    }

    /// Resolve `br depth` to `(label, kind)`.
    fn resolve_br(&self, depth: u32) -> (&str, &LabelBlockKind) {
        let idx = self.label_stack.len().saturating_sub(1 + depth as usize);
        let (kind, label) = &self.label_stack[idx];
        (label.as_str(), kind)
    }

    /// Write a (possibly shortened) name, quoting it if it contains special characters.
    fn write_name(&mut self, name: &str) {
        self.write(&quote_if_needed(name));
    }

    /// Split a possibly-`Seq` condition into `(preamble, actual_condition)`.
    ///
    /// If `condition` is `Seq([...preamble, last])`, returns `(preamble, last)`.
    /// Otherwise returns `(&[], condition)`.
    fn split_condition<'i>(&self, condition: &'i WirInstr) -> (&'i [WirInstr], &'i WirInstr) {
        if let WirInstr::Seq(instrs) = condition
            && let Some((last, preamble)) = instrs.split_last()
        {
            return (preamble, last);
        }
        (&[], condition)
    }

    /// Unwrap `if cond -> bool { 1 } else { 0 }` to just `cond`.
    ///
    /// This pattern is emitted by enum pattern matching to coerce a comparison
    /// result into a bool for use as an `if` condition.
    fn unwrap_bool_if<'i>(&self, instr: &'i WirInstr) -> &'i WirInstr {
        if let WirInstr::If {
            condition,
            result: Some(WirType::Bool),
            then_body,
            else_body: Some(else_body),
        } = instr
            && matches!(then_body.as_slice(), [WirInstr::I32Const(1)])
            && matches!(else_body.as_slice(), [WirInstr::I32Const(0)])
        {
            return condition.as_ref();
        }
        instr
    }

    /// Try to inline a scrutinee local into its single-use condition.
    ///
    /// Recognises the enum/variant pattern-match preamble:
    /// `[DeclareLocal(X, T), LocalSet(X, value)]` followed by `X == constant`.
    /// Returns a new `I32Eq(value, constant)` with the local eliminated, or
    /// `None` if the pattern does not match.
    fn try_inline_scrutinee(preamble: &[WirInstr], cond: &WirInstr) -> Option<WirInstr> {
        if preamble.len() != 2 {
            return None;
        }
        let WirInstr::DeclareLocal {
            name: decl_name, ..
        } = &preamble[0]
        else {
            return None;
        };
        let WirInstr::LocalSet {
            name: set_name,
            value,
        } = &preamble[1]
        else {
            return None;
        };
        if decl_name != set_name {
            return None;
        }
        if let WirInstr::I32Eq(a, b) = cond {
            if let WirInstr::LocalGet { name, .. } = a.as_ref()
                && name == decl_name
            {
                return Some(WirInstr::I32Eq(value.clone(), b.clone()));
            }
            if let WirInstr::LocalGet { name, .. } = b.as_ref()
                && name == decl_name
            {
                return Some(WirInstr::I32Eq(a.clone(), value.clone()));
            }
        }
        None
    }

    fn write(&mut self, s: &str) {
        self.output.push_str(s);
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent {
            self.output.push_str("    ");
        }
    }

    fn newline(&mut self) {
        self.output.push('\n');
    }
}

/// Format a `WirType` as a string for display.
pub fn format_type(ty: &WirType) -> String {
    match ty {
        WirType::I8 => "i8".to_string(),
        WirType::I16 => "i16".to_string(),
        WirType::I32 => "i32".to_string(),
        WirType::I64 => "i64".to_string(),
        WirType::U8 => "u8".to_string(),
        WirType::U16 => "u16".to_string(),
        WirType::U32 => "u32".to_string(),
        WirType::U64 => "u64".to_string(),
        WirType::F32 => "f32".to_string(),
        WirType::F64 => "f64".to_string(),
        WirType::V128 => "v128".to_string(),
        WirType::Bool => "bool".to_string(),
        WirType::Char => "char".to_string(),
        WirType::Unit => "()".to_string(),
        WirType::Enum { type_id } => type_id.to_string(),
        WirType::Flags { type_id } => type_id.to_string(),
        WirType::Ref { type_id, nullable } => {
            if *nullable {
                format!("ref null {type_id}")
            } else {
                format!("ref {type_id}")
            }
        }
        WirType::AbstractRef {
            heap_type,
            nullable,
        } => {
            let ht = format_abstract_heap_type(heap_type);
            if *nullable {
                format!("ref null {ht}")
            } else {
                format!("ref {ht}")
            }
        }
    }
}

/// Returns true if `instr` renders using infix operator syntax (needs parens as sub-expression).
fn is_op_instr(instr: &WirInstr) -> bool {
    use WirInstr::{
        F32Add, F32Div, F32Eq, F32Ge, F32Gt, F32Le, F32Lt, F32Mul, F32Ne, F32Sub, F64Add, F64Div,
        F64Eq, F64Ge, F64Gt, F64Le, F64Lt, F64Mul, F64Ne, F64Sub, I32Add, I32And, I32DivS, I32DivU,
        I32Eq, I32Eqz, I32GeS, I32GeU, I32GtS, I32GtU, I32LeS, I32LeU, I32LtS, I32LtU, I32Mul,
        I32Ne, I32Or, I32RemS, I32RemU, I32Shl, I32ShrS, I32ShrU, I32Sub, I32Xor, I64Add, I64And,
        I64DivS, I64DivU, I64Eq, I64Eqz, I64GeS, I64GeU, I64GtS, I64GtU, I64LeS, I64LeU, I64LtS,
        I64LtU, I64Mul, I64Ne, I64Or, I64RemS, I64RemU, I64Shl, I64ShrS, I64ShrU, I64Sub, I64Xor,
    };
    matches!(
        instr,
        I32Add(..)
            | I32Sub(..)
            | I32Mul(..)
            | I32DivS(..)
            | I32DivU(..)
            | I32RemS(..)
            | I32RemU(..)
            | I32And(..)
            | I32Or(..)
            | I32Xor(..)
            | I32Shl(..)
            | I32ShrS(..)
            | I32ShrU(..)
            | I32Eq(..)
            | I32Ne(..)
            | I32LtS(..)
            | I32LtU(..)
            | I32GtS(..)
            | I32GtU(..)
            | I32LeS(..)
            | I32LeU(..)
            | I32GeS(..)
            | I32GeU(..)
            | I32Eqz(..)
            | I64Add(..)
            | I64Sub(..)
            | I64Mul(..)
            | I64DivS(..)
            | I64DivU(..)
            | I64RemS(..)
            | I64RemU(..)
            | I64And(..)
            | I64Or(..)
            | I64Xor(..)
            | I64Shl(..)
            | I64ShrS(..)
            | I64ShrU(..)
            | I64Eq(..)
            | I64Ne(..)
            | I64LtS(..)
            | I64LtU(..)
            | I64GtS(..)
            | I64GtU(..)
            | I64LeS(..)
            | I64LeU(..)
            | I64GeS(..)
            | I64GeU(..)
            | I64Eqz(..)
            | F32Add(..)
            | F32Sub(..)
            | F32Mul(..)
            | F32Div(..)
            | F32Eq(..)
            | F32Ne(..)
            | F32Lt(..)
            | F32Gt(..)
            | F32Le(..)
            | F32Ge(..)
            | F64Add(..)
            | F64Sub(..)
            | F64Mul(..)
            | F64Div(..)
            | F64Eq(..)
            | F64Ne(..)
            | F64Lt(..)
            | F64Gt(..)
            | F64Le(..)
            | F64Ge(..)
    )
}

/// Returns true if any `br`/`br_if`/`br_table` in `body` targets the enclosing
/// block at `nesting` levels above the current position.
///
/// Call with `nesting = 0` to check whether a block's own body targets that block.
/// Recursing into nested blocks increments `nesting` so the check stays relative.
fn is_block_targeted_from(body: &[WirInstr], nesting: u32) -> bool {
    for instr in body {
        match instr {
            WirInstr::Br { depth } if *depth == nesting => return true,
            WirInstr::BrIf { depth, .. } if *depth == nesting => return true,
            WirInstr::BrTable {
                targets, default, ..
            } if targets.contains(&nesting) || *default == nesting => return true,
            WirInstr::Block { body, .. } | WirInstr::Loop { body, .. } => {
                if is_block_targeted_from(body, nesting + 1) {
                    return true;
                }
            }
            WirInstr::If {
                then_body,
                else_body,
                ..
            } => {
                if is_block_targeted_from(then_body, nesting + 1) {
                    return true;
                }
                if let Some(eb) = else_body
                    && is_block_targeted_from(eb, nesting + 1)
                {
                    return true;
                }
            }
            WirInstr::Seq(instrs) => {
                if is_block_targeted_from(instrs, nesting) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Returns true if `name` cannot be written as a bare word and needs double-quoting.
fn needs_quoting(name: &str) -> bool {
    name.is_empty() || name.contains('/') || name.contains(' ')
}

/// Wrap `name` in double quotes if it needs quoting, otherwise return it as-is.
fn quote_if_needed(name: &str) -> String {
    if needs_quoting(name) {
        format!("\"{name}\"")
    } else {
        name.to_string()
    }
}

/// Returns true if `instr` is not suitable for inlining as the value of a
/// `break label val;` expression. Multi-line constructs like `if`, `block`,
/// `loop`, and nested `Seq` are excluded to avoid garbled single-line output.
fn is_block_expr(instr: &WirInstr) -> bool {
    matches!(
        instr,
        WirInstr::If { .. } | WirInstr::Block { .. } | WirInstr::Loop { .. } | WirInstr::Seq(_)
    )
}

fn format_abstract_heap_type(ht: &WirAbstractHeapType) -> &'static str {
    match ht {
        WirAbstractHeapType::Any => "any",
        WirAbstractHeapType::Eq => "eq",
        WirAbstractHeapType::Struct => "struct",
        WirAbstractHeapType::Array => "array",
        WirAbstractHeapType::Func => "func",
        WirAbstractHeapType::None => "none",
        WirAbstractHeapType::NoFunc => "nofunc",
        WirAbstractHeapType::Extern => "extern",
    }
}
