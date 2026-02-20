//! WIR → pseudo-Wado unparser for `wado dump --wir --unparse`.
//!
//! Renders a `WirModule` as readable pseudo-Wado source code for debugging.
//! The output uses Wado syntax for type definitions (struct, variant, enum)
//! and WAT-style mnemonics for arithmetic instructions (i32.add, f64.mul, etc.).

use crate::wir::{
    WirAbstractHeapType, WirArrayType, WirEnumType, WirExport, WirExportDesc, WirField,
    WirFlagsType, WirFuncType, WirFunction, WirGlobal, WirImport, WirImportDesc, WirInstr,
    WirModule, WirStructType, WirType, WirTypeDef, WirVariantType,
};

/// Unparse a `WirModule` into pseudo-Wado source code.
///
/// `cwd` is the current working directory used to shorten entry-point paths.
pub fn unparse_wir(module: &WirModule, cwd: Option<&str>) -> String {
    let mut unparser = WirUnparser::new(module.entry_point_path.as_deref(), cwd, &module.types);
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
    /// Entry-point module path (to detect and shorten in FQ names).
    entry_point_path: Option<String>,
    /// Type definitions for struct field name lookup.
    types: &'a [WirTypeDef],
    /// Stack of (kind, label) for block-depth tracking and `br N` resolution.
    label_stack: Vec<(LabelBlockKind, String)>,
    /// Counter for generating unique labels.
    label_next_id: usize,
}

impl<'a> WirUnparser<'a> {
    fn new(entry_point_path: Option<&str>, _cwd: Option<&str>, types: &'a [WirTypeDef]) -> Self {
        Self {
            output: String::new(),
            indent: 0,
            entry_point_path: entry_point_path.map(String::from),
            types,
            label_stack: Vec::new(),
            label_next_id: 0,
        }
    }

    /// Get the element type of a GC array type as a display string.
    fn array_elem_type_str(&self, type_id: &crate::wir::WirTypeId) -> String {
        let idx = type_id.index() as usize;
        if let Some(WirTypeDef::Array(a)) = self.types.get(idx) {
            self.fmt_type(&a.element_type)
        } else {
            // Fallback: strip "array<...>" wrapper from the display name.
            let tid = self.shorten_fq(&type_id.to_string());
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

    /// Shorten a fully-qualified WIR name for display.
    ///
    /// FQ names follow the format `"module_source//name"`.
    /// - Entry-point items → just the name (e.g., `Point`, `run`)
    /// - `core:prelude/string.wado//String` → `String`
    /// - `core:cli//println` → `core:cli/println`
    /// - `builtin::array<u8>` → `array<u8>`
    fn shorten_fq(&self, fq: &str) -> String {
        // Handle "builtin::" prefix
        if let Some(rest) = fq.strip_prefix("builtin::") {
            return rest.to_string();
        }

        // Split on "//" separator
        if let Some((module, name)) = fq.split_once("//") {
            let short_module = self.shorten_module_path(module);
            // Also shorten the name part (it may contain module paths, e.g. functype names)
            let short_name = self.shorten_nested_path(name);
            if short_module.is_empty() {
                short_name
            } else {
                format!("{short_module}/{short_name}")
            }
        } else {
            // No "//" separator — check for entry-point path as prefix
            self.shorten_nested_path(fq)
        }
    }

    /// Shorten nested module paths within a name segment.
    ///
    /// E.g., `"wado-compiler/tests/fixtures/geometry.wado/run"` → `"run"`
    /// E.g., `"core:prelude/string.wado/String::grow"` → `"String::grow"`
    fn shorten_nested_path(&self, name: &str) -> String {
        // Replace entry-point path prefix
        if let Some(ref ep) = self.entry_point_path
            && let Some(rest) = name.strip_prefix(ep.as_str())
        {
            if let Some(func) = rest.strip_prefix('/') {
                return func.to_string();
            }
            // Exact match (no trailing /)
            if rest.is_empty() {
                return name.to_string();
            }
        }
        // Replace core:prelude/* prefix
        if name.starts_with("core:prelude") {
            // Find the "//" or last "/" after the .wado extension
            if let Some(pos) = name.find(".wado/") {
                return name[pos + 6..].to_string();
            }
        }
        name.to_string()
    }

    /// Shorten a module path for display.
    fn shorten_module_path(&self, module: &str) -> String {
        // Entry-point path → empty (items display as just name)
        if let Some(ref ep) = self.entry_point_path
            && module == ep
        {
            return String::new();
        }
        // Absolute paths → canonicalize to ./filename.wado
        if module.starts_with('/') {
            return crate::name::canonicalize_entry_point(module);
        }
        // core:prelude/* → omit for well-known prelude types
        if module.starts_with("core:prelude") {
            return String::new();
        }
        // Already short (core:cli, wasi:*, etc.)
        module.to_string()
    }

    fn unparse(&mut self, module: &WirModule) {
        // Type definitions
        for type_def in &module.types {
            self.unparse_type_def(type_def);
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

    fn unparse_type_def(&mut self, type_def: &WirTypeDef) {
        match type_def {
            WirTypeDef::Struct(s) => self.unparse_struct_type(s),
            WirTypeDef::Variant(v) => self.unparse_variant_type(v),
            WirTypeDef::Enum(e) => self.unparse_enum_type(e),
            WirTypeDef::Flags(f) => self.unparse_flags_type(f),
            WirTypeDef::Array(a) => self.unparse_array_type(a),
            WirTypeDef::Func(f) => self.unparse_func_type(f),
        }
    }

    fn unparse_struct_type(&mut self, s: &WirStructType) {
        self.write_indent();
        self.write("struct ");
        self.write(&s.name.display);
        self.write(" {");

        self.unparse_source_comment(&s.meta);

        if let Some(ref origin) = s.generic_origin {
            self.write(&format!(
                "  // {}<T> with T={}",
                origin.base_name,
                origin.type_args.join(", ")
            ));
        }

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

    fn unparse_variant_type(&mut self, v: &WirVariantType) {
        self.write_indent();
        self.write("variant ");
        self.write(&v.name.display);
        self.write(" {");
        self.unparse_source_comment(&v.meta);
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

    fn unparse_enum_type(&mut self, e: &WirEnumType) {
        self.write_indent();
        self.write("enum ");
        self.write(&e.name.display);
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
        self.unparse_source_comment(&e.meta);
        self.newline();
    }

    fn unparse_flags_type(&mut self, f: &WirFlagsType) {
        self.write_indent();
        self.write("flags ");
        self.write(&f.name.display);
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
        self.unparse_source_comment(&f.meta);
        self.newline();
    }

    fn unparse_array_type(&mut self, a: &WirArrayType) {
        self.write_indent();
        self.write("array ");
        self.write(&a.name.display);
        if a.mutable {
            self.write(" (mut ");
        } else {
            self.write(" (");
        }
        self.write(&self.fmt_type(&a.element_type));
        self.write(");");
        self.unparse_source_comment(&a.meta);
        self.newline();
    }

    fn unparse_func_type(&mut self, f: &WirFuncType) {
        self.write_indent();
        self.write("type ");
        let name = self.shorten_fq(&f.name.display);
        self.write_name(&name);
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
        self.write(";");
        self.newline();
    }

    // === Imports ===

    fn unparse_import(&mut self, import: &WirImport) {
        self.write_indent();
        self.write("import ");
        match &import.desc {
            WirImportDesc::Func { name, .. } => {
                self.write("fn ");
                self.write(&name.display);
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
        self.write(&global.name.display);
        self.write(": ");
        self.write(&self.fmt_type(&global.ty));
        self.write(" = ");
        self.unparse_instr_inline(&global.init);
        self.write(";");
        self.unparse_source_comment(&global.meta);
        self.newline();
    }

    // === Functions ===

    fn unparse_function(&mut self, func: &WirFunction) {
        self.write_indent();
        self.write("fn ");
        let short_name = self.shorten_fq(&func.name.fq);
        self.write_name(&short_name);
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
            self.write(&func.effects.join(", "));
        }

        self.write(" {");
        self.unparse_source_comment(&func.meta);
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
            self.write(&name.display);
            self.write(" = ");
            self.unparse_instr_inline(last_val);
            self.write(";");
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
            WirInstr::LocalGet { name } => {
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
            WirInstr::GlobalGet { name } => {
                self.write(&name.display);
            }
            WirInstr::GlobalSet { name, value } => {
                self.write(&name.display);
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

            // GC: Struct
            WirInstr::StructNew { type_id, fields } => {
                let tid = self.shorten_fq(&type_id.to_string());
                let field_names = self.struct_field_names(type_id);
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
                self.write(&format!("array.new_data<{elem}>[{data_index}]("));
                self.unparse_instr_inline(offset);
                self.write(", ");
                self.unparse_instr_inline(len);
                self.write(")");
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
                let tid = self.shorten_fq(&type_id.to_string());
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
                let tid = self.shorten_fq(&type_id.to_string());
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
                    self.unparse_instr_inline(v);
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
                let fid = self.shorten_fq(&func_id.to_string());
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
                let tid = self.shorten_fq(&type_id.to_string());
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
                let tid = self.shorten_fq(&type_id.to_string());
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
                let fid = self.shorten_fq(&func_id.to_string());
                self.write("ref.func ");
                self.write_name(&fid);
            }

            // Memory
            WirInstr::MemorySize => self.write("builtin::memory_size()"),
            WirInstr::MemoryGrow(a) => self.write_unop("builtin::memory_grow", a),
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

            // Compound
            WirInstr::ValueCopy { type_id, expr, .. } => {
                let tid = self.shorten_fq(&type_id.to_string());
                self.write("value_copy ");
                self.write_name(&tid);
                self.write("(");
                self.unparse_instr_inline(expr);
                self.write(")");
            }
            WirInstr::MultiValueStructNew { type_id, instr } => {
                let tid = self.shorten_fq(&type_id.to_string());
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
                let fid = self.shorten_fq(&func_id.to_string());
                self.write("fn ");
                self.write_name(&fid);
            }
            WirExportDesc::Global { name } => {
                self.write(&format!("global {}", name.display));
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

    fn unparse_source_comment(&mut self, meta: &crate::wir::WirMeta) {
        if let Some(ref source) = meta.module_source {
            let short = self.shorten_module_path(&source.to_string());
            if !short.is_empty() {
                self.write(&format!("  // from {short}"));
            }
            // Omit comment for entry-point module (empty short means entry point)
        }
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
            WirType::Bool => "bool".to_string(),
            WirType::Char => "char".to_string(),
            WirType::Unit => "()".to_string(),
            WirType::Enum { type_id } => quote_if_needed(&self.shorten_fq(&type_id.to_string())),
            WirType::Flags { type_id } => quote_if_needed(&self.shorten_fq(&type_id.to_string())),
            WirType::Ref { type_id, nullable } => {
                let short = quote_if_needed(&self.shorten_fq(&type_id.to_string()));
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
            if let WirInstr::LocalGet { name } = a.as_ref()
                && name == decl_name
            {
                return Some(WirInstr::I32Eq(value.clone(), b.clone()));
            }
            if let WirInstr::LocalGet { name } = b.as_ref()
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
