//! NIR → pseudo-Wado renderer for `wado dump --nir`.
//!
//! Initially a renamed copy of [`crate::unparse`]'s `TirUnparser`. See
//! `docs/wep-2026-05-11-nir.md`.

use crate::lexer::is_valid_ident;
use crate::nir::{
    NirBinaryOp, NirBlock, NirEnum, NirExpr, NirExprKind, NirFlags, NirFunction, NirGlobal,
    NirLiteralPattern, NirModule, NirParam, NirPattern, NirStmt, NirStmtKind, NirStruct,
    NirUnaryOp,
};
use crate::tir::{TypeId, TypeTable};

fn escape_string(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\0' => result.push_str("\\0"),
            c if c.is_control() => {
                result.push_str(&format!("\\u{{{:04X}}}", c as u32));
            }
            c => result.push(c),
        }
    }
    result
}

fn escape_char(c: char) -> String {
    match c {
        '\'' => "\\'".to_string(),
        '\\' => "\\\\".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        '\0' => "\\0".to_string(),
        c if c.is_control() => format!("\\u{{{:04X}}}", c as u32),
        c => c.to_string(),
    }
}

fn format_spec_to_string(spec: &crate::nir::TemplateFormatSpec) -> String {
    let mut s = String::new();
    if let Some(fill) = spec.fill {
        s.push(fill);
    }
    if let Some(align) = spec.align {
        s.push(align);
    }
    if spec.sign_plus {
        s.push('+');
    }
    if spec.alternate {
        s.push('#');
    }
    if spec.zero_pad {
        s.push('0');
    }
    if let Some(w) = spec.width {
        s.push_str(&w.to_string());
    }
    if let Some(p) = spec.precision {
        s.push('.');
        s.push_str(&p.to_string());
    }
    if let Some(t) = spec.type_char {
        s.push(t);
    }
    s
}

/// Unparses NIR back to pseudo-Wado source code.
/// The output shows the code after monomorphization and lowering.
/// Note: Monomorphized names like `Box<i32>` are quoted to make the output parseable.
pub struct NirUnparser<'a> {
    type_table: &'a TypeTable,
    output: String,
    indent_level: usize,
    /// When true, suppress internal debug annotations (e.g.,
    /// `@capture[0]:n` becomes `n`). Used for closure source-form
    /// rendering exposed to user-facing inspect output. Default
    /// `false` keeps the debug-friendly form for NIR dumps.
    source_form: bool,
}

impl<'a> NirUnparser<'a> {
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self {
            type_table,
            output: String::new(),
            indent_level: 0,
            source_form: false,
        }
    }

    /// Enable source-form rendering: suppresses internal annotations
    /// like `@capture[i]:` so the output reflects user-written names.
    fn source_form(mut self) -> Self {
        self.source_form = true;
        self
    }

    /// Quote an identifier if it contains characters that make it invalid Wado syntax.
    /// Valid Wado identifiers match /^[a-zA-Z_][a-zA-Z0-9_]*$/
    /// `::` is allowed as a namespace separator (each segment must be a valid identifier).
    /// Names with `<`, `>`, `,` (monomorphized), `^` (trait), `/`, `.` need quoting.
    fn quote_if_needed(name: &str) -> String {
        if name.split("::").all(is_valid_ident) {
            name.to_string()
        } else {
            format!("\"{name}\"")
        }
    }

    fn emit_kw_if(&mut self, cond: bool, kw: &str) {
        if cond {
            self.output.push_str(kw);
        }
    }

    /// Emit `f(self, item)` for each item separated by `", "`.
    fn comma_sep<I, F>(&mut self, items: I, f: F)
    where
        I: IntoIterator,
        F: FnMut(&mut Self, I::Item),
    {
        self.comma_sep_with(", ", items, f);
    }

    /// Like `comma_sep`, but with a custom `separator` between items.
    fn comma_sep_with<I, F>(&mut self, separator: &str, items: I, mut f: F)
    where
        I: IntoIterator,
        F: FnMut(&mut Self, I::Item),
    {
        for (i, item) in items.into_iter().enumerate() {
            if i > 0 {
                self.output.push_str(separator);
            }
            f(self, item);
        }
    }

    /// Emit `open`, comma-separated items, then `close`.
    fn delimited<I, F>(&mut self, open: &str, close: &str, items: I, f: F)
    where
        I: IntoIterator,
        F: FnMut(&mut Self, I::Item),
    {
        self.output.push_str(open);
        self.comma_sep(items, f);
        self.output.push_str(close);
    }

    /// Emit a turbofish `::<T1, T2, ...>` for a list of monomorphized type ids.
    fn unparse_type_args(&mut self, args: &[crate::tir::TypeId]) {
        if args.is_empty() {
            return;
        }
        self.delimited("::<", ">", args, |s, t| {
            let name = s.type_table.type_name(*t);
            s.output.push_str(&name);
        });
    }

    /// Emit a `{ ... body ... }` block at one extra indent level.
    fn emit_indented_block<F>(&mut self, body: F)
    where
        F: FnOnce(&mut Self),
    {
        self.output.push_str(" {\n");
        self.indent_level += 1;
        body(self);
        self.indent_level -= 1;
        self.write_indent();
        self.output.push('}');
    }

    pub fn unparse(mut self, module: &NirModule) -> String {
        self.unparse_module(module);
        self.output
    }

    fn unparse_module(&mut self, module: &NirModule) {
        if !module.imports.is_empty() {
            self.output.push_str("// Imports\n");
            for import in &module.imports {
                self.output.push_str("// ");
                self.output.push_str(&import.namespace);
                self.output.push_str("::");
                self.output.push_str(&import.canonical_name);
                self.output.push('\n');
            }
            self.output.push('\n');
        }

        for g in &module.globals {
            self.unparse_nir_global(g);
            self.output.push('\n');
        }

        for s in &module.structs {
            self.unparse_struct(s);
            self.output.push('\n');
        }

        for e in &module.enums {
            self.unparse_enum(e);
            self.output.push('\n');
        }

        for f in &module.flags {
            self.unparse_flags_tir(f);
            self.output.push('\n');
        }

        for f_rc in &module.functions {
            let f = f_rc.borrow();
            self.unparse_function(&f);
            self.output.push('\n');
        }

        if let Some(data) = &module.data_section {
            self.output.push_str("__DATA__\n");
            self.output.push_str(data);
        }
    }

    fn unparse_nir_global(&mut self, g: &NirGlobal) {
        self.write_indent();
        self.emit_kw_if(g.is_pub, "pub ");
        self.output.push_str("global ");
        self.emit_kw_if(g.mutable, "mut ");
        self.output.push_str(&g.name);
        self.output.push_str(": ");
        self.output.push_str(&self.type_table.type_name(g.ty));
        self.output.push_str(" = ");
        self.unparse_expr(&g.initializer);
        self.output.push_str(";\n");
    }

    fn unparse_struct(&mut self, s: &NirStruct) {
        self.write_indent();
        self.emit_kw_if(s.is_pub, "pub ");
        self.output.push_str("struct ");
        self.output.push_str(&Self::quote_if_needed(&s.name));

        // Generic params (only present for unmonomorphized structs).
        if !s.type_params.is_empty() {
            self.delimited("<", ">", &s.type_params, |s, param| {
                s.output.push_str(&param.name);
                if !param.bounds.is_empty() {
                    s.output.push_str(": ");
                    s.output.push_str(&param.bounds.join(" + "));
                }
                if let Some(default_type) = param.default {
                    s.output.push_str(" = ");
                    let name = s.type_table.type_name(default_type);
                    s.output.push_str(&name);
                }
            });
        }

        self.emit_indented_block(|this| {
            for field in &s.fields {
                this.write_indent();
                this.emit_kw_if(field.is_pub, "pub ");
                this.output.push_str(&field.name);
                this.output.push_str(": ");
                this.output
                    .push_str(&this.type_table.type_name(field.type_id));
                this.output.push_str(",\n");
            }
        });
        self.output.push('\n');
    }

    fn unparse_enum(&mut self, e: &NirEnum) {
        self.write_indent();
        self.emit_kw_if(e.is_pub, "pub ");
        self.output.push_str("enum ");
        self.output.push_str(&e.name);
        self.emit_indented_block(|this| {
            for case in &e.cases {
                this.write_indent();
                this.output.push_str(&case.name);
                // Enum cases have no payload (unlike variant cases)
                this.output.push_str(",\n");
            }
        });
        self.output.push('\n');
    }

    fn unparse_flags_tir(&mut self, f: &NirFlags) {
        self.write_indent();
        self.emit_kw_if(f.is_pub, "pub ");
        self.output.push_str("flags ");
        self.output.push_str(&f.name);
        self.emit_indented_block(|this| {
            for member in &f.members {
                this.write_indent();
                this.output.push_str(&member.name);
                this.output.push_str(",  // 0x");
                this.output.push_str(&format!("{:x}", member.bitmask));
                this.output.push('\n');
            }
        });
        self.output.push('\n');
    }

    fn unparse_function(&mut self, f: &NirFunction) {
        if let Some(attr) = inline_hint_attr(f.inline_hint) {
            self.write_indent();
            self.output.push_str(attr);
            self.output.push('\n');
        }
        self.write_indent();
        self.emit_kw_if(f.is_pub, "pub ");
        self.emit_kw_if(f.is_export, "export ");
        self.output.push_str("fn ");
        self.output.push_str(&Self::quote_if_needed(&f.name));

        // Generic params (only present for unmonomorphized functions).
        if !f.type_params.is_empty() {
            self.delimited("<", ">", &f.type_params, |s, param| {
                s.output.push_str(&param.name);
                if !param.bounds.is_empty() {
                    s.output.push_str(": ");
                    s.output.push_str(&param.bounds.join(" + "));
                }
                if let Some(default_type) = param.default {
                    s.output.push_str(" = ");
                    let name = s.type_table.type_name(default_type);
                    s.output.push_str(&name);
                }
            });
        }

        self.delimited("(", ")", &f.params, NirUnparser::unparse_param);

        if f.return_type != TypeTable::UNIT {
            self.output.push_str(" -> ");
            self.output
                .push_str(&self.type_table.type_name(f.return_type));
        }

        self.unparse_nir_with_clause(&f.effects, &f.stores);

        if let Some(body) = &f.body {
            self.emit_indented_block(|this| this.unparse_block(body));
            self.output.push('\n');
        } else {
            self.output.push_str(";\n");
        }
    }

    fn unparse_nir_with_clause(&mut self, effects: &[crate::tir::EffectRef], stores: &[String]) {
        if effects.is_empty() && stores.is_empty() {
            return;
        }
        self.output.push_str(" with ");
        if !effects.is_empty() {
            self.comma_sep(effects, |s, e| s.output.push_str(e.name()));
            if !stores.is_empty() {
                self.output.push_str(", ");
            }
        }
        if !stores.is_empty() {
            self.output.push_str("stores[");
            self.output.push_str(&stores.join(", "));
            self.output.push(']');
        }
    }

    fn unparse_param(&mut self, param: &NirParam) {
        self.output.push_str(&param.name);
        self.output.push_str(": ");
        self.output
            .push_str(&self.type_table.type_name(param.type_id));
    }

    fn unparse_block(&mut self, block: &NirBlock) {
        for stmt in &block.stmts {
            self.unparse_stmt(stmt);
        }
    }

    fn unparse_stmt(&mut self, stmt: &NirStmt) {
        match &stmt.kind {
            NirStmtKind::Let {
                name,
                is_mut,
                is_reactive,
                type_id,
                value,
                ..
            } => {
                self.write_indent();
                self.output.push_str("let ");
                if *is_reactive {
                    self.output.push_str("reactive ");
                }
                if *is_mut {
                    self.output.push_str("mut ");
                }
                self.output.push_str(name);
                self.output.push_str(": ");
                self.output.push_str(&self.type_table.type_name(*type_id));
                self.output.push_str(" = ");
                self.unparse_expr(value);
                self.output.push_str(";\n");
            }
            NirStmtKind::Expr(expr) => {
                self.write_indent();
                self.unparse_expr(expr);
                self.output.push_str(";\n");
            }
            NirStmtKind::Return { value } => {
                self.write_indent();
                self.output.push_str("return");
                if let Some(v) = value {
                    self.output.push(' ');
                    self.unparse_expr(v);
                }
                self.output.push_str(";\n");
            }
            NirStmtKind::TaskReturn { value } => {
                self.write_indent();
                self.output.push_str("task return ");
                self.unparse_expr(value);
                self.output.push_str(";\n");
            }
            NirStmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                self.write_indent();
                self.output.push_str("if ");
                self.unparse_expr(condition);
                self.output.push_str(" {\n");
                self.indent_level += 1;
                self.unparse_block(then_block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
                if let Some(else_blk) = else_block {
                    self.output.push_str(" else {\n");
                    self.indent_level += 1;
                    self.unparse_block(else_blk);
                    self.indent_level -= 1;
                    self.write_indent();
                    self.output.push('}');
                }
                self.output.push('\n');
            }
            NirStmtKind::Loop { body } => {
                self.write_indent();
                self.output.push_str("loop {\n");
                self.indent_level += 1;
                self.unparse_block(body);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
            }
            NirStmtKind::Break { label, value } => {
                self.write_indent();
                self.output.push_str("break");
                if let Some(lbl) = label {
                    self.output.push(' ');
                    self.output.push_str(lbl);
                    if let Some(val) = value {
                        self.output.push_str(": ");
                        self.unparse_expr(val);
                    }
                }
                self.output.push_str(";\n");
            }
            NirStmtKind::Continue => {
                self.write_indent();
                self.output.push_str("continue;\n");
            }
            NirStmtKind::LabeledBlock { label, block } => {
                self.write_indent();
                self.output.push_str(label);
                self.output.push_str(": {\n");
                self.indent_level += 1;
                self.unparse_block(block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
            }
            NirStmtKind::IfLet {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => {
                self.write_indent();
                self.output.push_str("if ");
                self.unparse_nir_pattern(pattern);
                self.output.push_str(" = ");
                self.unparse_expr(scrutinee);
                self.output.push_str(" {\n");
                self.indent_level += 1;
                self.unparse_block(then_block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
                if let Some(else_blk) = else_block {
                    self.output.push_str(" else {\n");
                    self.indent_level += 1;
                    self.unparse_block(else_blk);
                    self.indent_level -= 1;
                    self.write_indent();
                    self.output.push('}');
                }
                self.output.push('\n');
            }
            NirStmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => {
                self.write_indent();
                self.output.push_str("let ");
                if *is_mut {
                    self.output.push_str("mut ");
                }
                self.unparse_nir_pattern(pattern);
                self.output.push_str(" = ");
                self.unparse_expr(value);
                self.output.push_str(";\n");
            }
            NirStmtKind::VariadicForOf {
                iterable,
                binding_name,
                is_mut,
                body,
                ..
            } => {
                self.write_indent();
                self.output.push_str("for let ");
                if *is_mut {
                    self.output.push_str("mut ");
                }
                self.output.push_str(binding_name);
                self.output.push_str(" of <variadic> ");
                self.unparse_expr(iterable);
                self.output.push_str(" {\n");
                self.indent_level += 1;
                for stmt in &body.stmts {
                    self.unparse_stmt(stmt);
                }
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
            }
        }
    }

    fn unparse_nir_pattern(&mut self, pattern: &NirPattern) {
        match pattern {
            NirPattern::Wildcard => self.output.push('_'),
            NirPattern::Binding { name, .. } => self.output.push_str(name),
            NirPattern::Literal(lit) => emit_tir_literal_pattern(lit, &mut self.output),
            NirPattern::Tuple(patterns, has_rest) => {
                self.output.push('[');
                self.comma_sep(patterns, NirUnparser::unparse_nir_pattern);
                if *has_rest {
                    if !patterns.is_empty() {
                        self.output.push_str(", ");
                    }
                    self.output.push_str("..");
                }
                self.output.push(']');
            }
            NirPattern::Variant {
                variant_name,
                bindings,
                ..
            } => {
                self.output.push_str(variant_name);
                if !bindings.is_empty() {
                    self.delimited("(", ")", bindings, NirUnparser::unparse_nir_pattern);
                }
            }
            NirPattern::Enum { case_name, .. } => self.output.push_str(case_name),
            NirPattern::Struct { fields, .. } => {
                self.output.push_str("{ ");
                self.comma_sep(fields, |s, field| {
                    s.output.push_str(&field.field_name);
                    if !matches!(&field.pattern, NirPattern::Binding { name, .. } if name == &field.field_name)
                    {
                        s.output.push_str(": ");
                        s.unparse_nir_pattern(&field.pattern);
                    }
                });
                self.output.push_str(" }");
            }
            NirPattern::Or(alternatives) => {
                self.comma_sep_with(" | ", alternatives, NirUnparser::unparse_nir_pattern);
            }
            NirPattern::ConstantValue { expr } => self.unparse_expr(expr),
            NirPattern::Range {
                start,
                end,
                inclusive,
                ..
            } => {
                self.output.push_str(&start.to_string());
                self.output.push_str(if *inclusive { "..=" } else { "..<" });
                self.output.push_str(&end.to_string());
            }
        }
    }

    fn unparse_expr(&mut self, expr: &NirExpr) {
        match &expr.kind {
            NirExprKind::IntLiteral { repr, .. } => {
                self.output.push_str(repr);
            }
            NirExprKind::FloatLiteral { repr, .. } => {
                self.output.push_str(repr);
            }
            NirExprKind::BoolLiteral(b) => {
                self.output.push_str(if *b { "true" } else { "false" });
            }
            NirExprKind::CharLiteral(c) => {
                self.output.push('\'');
                self.output.push_str(&escape_char(*c));
                self.output.push('\'');
            }
            NirExprKind::StringLiteral(s) => {
                self.output.push('"');
                self.output.push_str(&escape_string(s));
                self.output.push('"');
            }
            NirExprKind::BytesLiteral(bytes) => {
                self.output
                    .push_str(&format!("#include_bytes(/* {} bytes */)", bytes.len()));
            }
            NirExprKind::Null => {
                self.output.push_str("null");
            }
            NirExprKind::VariantConstruct {
                case_name, payload, ..
            } => {
                // Get the variant type name from the type_id
                let type_name = self.type_table.type_name(expr.type_id);
                self.output.push_str(&type_name);
                self.output.push_str("::");
                self.output.push_str(case_name);
                if let Some(payload_expr) = payload {
                    self.output.push('(');
                    self.unparse_expr(payload_expr);
                    self.output.push(')');
                }
            }
            NirExprKind::EnumConstruct { case_name, .. } => {
                // Get the enum type name from the type_id
                let type_name = self.type_table.type_name(expr.type_id);
                self.output.push_str(&type_name);
                self.output.push_str("::");
                self.output.push_str(case_name);
            }
            NirExprKind::Unit => {
                self.output.push_str("()");
            }
            NirExprKind::Local { name, .. } => {
                self.output.push_str(name);
            }
            NirExprKind::FuncRef {
                name,
                module_source,
            } => {
                if !module_source.is_entry_point() {
                    self.output.push_str(&module_source.to_path().join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(name);
            }
            NirExprKind::GlobalVarGet {
                name,
                module_source,
            } => {
                if !module_source.is_entry_point() {
                    self.output.push_str(&module_source.to_path().join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(name);
            }
            NirExprKind::GlobalVarSet {
                name,
                module_source,
                value,
            } => {
                if !module_source.is_entry_point() {
                    self.output.push_str(&module_source.to_path().join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(name);
                self.output.push_str(" = ");
                self.unparse_expr(value);
            }
            NirExprKind::Capture { name, index } => {
                if self.source_form {
                    // Source-form: just the original name. The presence of
                    // a captures[...] header (rendered separately) already
                    // signals that the closure depends on captured locals.
                    self.output.push_str(name);
                } else {
                    // Debug-form: include capture index for NIR dumps.
                    self.output.push_str(&format!("@capture[{index}]:{name}"));
                }
            }
            NirExprKind::Binary { left, op, right } => {
                self.output.push('(');
                self.unparse_expr(left);
                self.output.push(' ');
                self.output.push_str(nir_binary_op_str(*op));
                self.output.push(' ');
                self.unparse_expr(right);
                self.output.push(')');
            }
            NirExprKind::Unary { op, expr: inner } => {
                self.output.push_str(nir_unary_op_str(*op));
                self.unparse_expr(inner);
            }
            NirExprKind::Assign { target, value } => {
                self.unparse_expr(target);
                self.output.push_str(" = ");
                self.unparse_expr(value);
            }
            NirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                self.unparse_expr(inner);
                self.output.push_str(" as ");
                self.output
                    .push_str(&self.type_table.type_name(*target_type));
            }
            NirExprKind::Call {
                func,
                type_args,
                args,
                ..
            } => {
                let func_name = func.name.clone();
                let full_name = if func.module_source.clone().is_entry_point() {
                    func_name
                } else {
                    let module_path = func.module_path();
                    format!("{}::{func_name}", module_path.join("::"))
                };
                self.output.push_str(&Self::quote_if_needed(&full_name));
                self.unparse_type_args(type_args);
                self.delimited("(", ")", args, |s, arg| s.unparse_expr(&arg.expr));
            }
            NirExprKind::CmRawCall { local_name, args } => {
                self.output.push_str("cm_raw_call ");
                self.output.push_str(local_name);
                self.delimited("(", ")", args, NirUnparser::unparse_expr);
            }
            NirExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
                ..
            } => {
                // The resolver wraps `self` receivers in `&`/`&mut` automatically;
                // strip that wrapper so the rendering reflects the source value.
                let actual_receiver = match &receiver.kind {
                    NirExprKind::Unary {
                        op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                        expr: inner,
                    } => inner.as_ref(),
                    _ => receiver.as_ref(),
                };
                self.unparse_expr(actual_receiver);
                self.output.push('.');
                // Quote the full resolved method name (e.g. `"Type::method"`) so
                // the output captures which impl was selected.
                self.output.push_str(&Self::quote_if_needed(&func.name));
                self.unparse_type_args(type_args);
                self.delimited("(", ")", args, |s, arg| s.unparse_expr(&arg.expr));
            }
            NirExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => {
                self.unparse_expr(inner);
                self.output.push('.');
                self.output.push_str(field_name);
            }
            NirExprKind::Index { expr: array, index } => {
                self.unparse_expr(array);
                self.output.push('[');
                self.unparse_expr(index);
                self.output.push(']');
            }
            NirExprKind::Block(block) => {
                self.output.push_str("{\n");
                self.indent_level += 1;
                self.unparse_block(block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
            }
            NirExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.output.push_str("if ");
                self.unparse_expr(condition);
                self.output.push_str(" {\n");
                self.indent_level += 1;
                self.unparse_block(then_branch);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
                if let Some(else_blk) = else_branch {
                    self.output.push_str(" else {\n");
                    self.indent_level += 1;
                    self.unparse_block(else_blk);
                    self.indent_level -= 1;
                    self.write_indent();
                    self.output.push('}');
                }
            }
            NirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.output.push_str("match ");
                self.unparse_expr(scrutinee);
                self.emit_indented_block(|this| {
                    for arm in arms {
                        this.write_indent();
                        this.unparse_nir_pattern(&arm.pattern);
                        if let Some(guard) = &arm.guard {
                            this.output.push_str(" && ");
                            this.unparse_expr(guard);
                        }
                        this.output.push_str(" => ");
                        this.unparse_expr(&arm.body);
                        this.output.push_str(",\n");
                    }
                });
            }
            NirExprKind::StructLiteral {
                struct_name,
                fields,
                ..
            } => {
                // Functor structs are rendered as `&Name { ... }` to mirror the
                // reference type that the resolver attached.
                if matches!(
                    self.type_table.get(expr.type_id),
                    crate::tir::ResolvedType::Ref(_)
                ) {
                    self.output.push('&');
                }
                self.output.push_str(struct_name);
                self.output.push_str(" { ");
                self.comma_sep(fields, |s, field| {
                    s.output.push_str(&field.name);
                    s.output.push_str(": ");
                    s.unparse_expr(&field.value);
                });
                self.output.push_str(" }");
            }
            NirExprKind::TupleLiteral { elements } => {
                self.delimited("[", "]", elements, NirUnparser::unparse_expr);
            }
            NirExprKind::TupleSpread { expr } | NirExprKind::TupleZip { expr } => {
                self.output.push_str("[..");
                self.unparse_expr(expr);
                self.output.push(']');
            }
            NirExprKind::TypePackExpansion { call_expr, .. } => {
                self.output.push_str("[..");
                self.unparse_expr(call_expr);
                self.output.push(']');
            }
            NirExprKind::Closure {
                params,
                body,
                captures,
                ..
            } => {
                self.unparse_closure_form(params, captures, body);
            }
            NirExprKind::IndirectCall { callee, args } => {
                self.unparse_expr(callee);
                self.delimited("(", ")", args, NirUnparser::unparse_expr);
            }
            NirExprKind::ClosureToCanonical { functor, .. } => {
                // Just unparse the functor - the canonical wrapper is invisible
                self.unparse_expr(functor);
            }
            NirExprKind::LabeledBlock { label, block, .. } => {
                self.output.push_str(label);
                self.output.push_str(": {\n");
                self.indent_level += 1;
                self.unparse_block(block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
            }

            // Lowered pattern matching nodes
            NirExprKind::VariantTag { expr } => {
                self.output.push_str("__variant_tag(");
                self.unparse_expr(expr);
                self.output.push(')');
            }
            NirExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => {
                self.output.push_str("__variant_test(");
                self.unparse_expr(expr);
                self.output
                    .push_str(&format!(", case={case_index}, name={case_name})"));
            }
            NirExprKind::VariantPayload {
                expr, case_index, ..
            } => {
                self.output.push_str("__variant_payload(");
                self.unparse_expr(expr);
                self.output.push_str(&format!(", case={case_index})"));
            }
            NirExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => {
                self.output.push_str("switch ");
                self.unparse_expr(scrutinee);
                self.output.push_str(&format!(" (base={min_value}) {{\n"));
                self.indent_level += 1;
                for (i, arm) in arms.iter().enumerate() {
                    self.write_indent();
                    self.output
                        .push_str(&format!("{} => {{\n", *min_value + i as i64));
                    self.indent_level += 1;
                    self.unparse_block(arm);
                    self.indent_level -= 1;
                    self.write_indent();
                    self.output.push_str("}\n");
                }
                self.write_indent();
                self.output.push_str("_ => {\n");
                self.indent_level += 1;
                self.unparse_block(default);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
            }
            NirExprKind::TemplateString { parts } => {
                self.output.push('`');
                for part in parts {
                    match part {
                        crate::nir::NirTemplatePart::Literal(s) => {
                            self.output.push_str(s);
                        }
                        crate::nir::NirTemplatePart::Interpolation {
                            expr: inner,
                            format_spec,
                        } => {
                            self.output.push('{');
                            self.unparse_expr(inner);
                            if let Some(spec) = format_spec {
                                self.output.push(':');
                                self.output.push_str(&format_spec_to_string(spec));
                            }
                            self.output.push('}');
                        }
                    }
                }
                self.output.push('`');
            }
            NirExprKind::WithHandler { bindings, body, .. } => {
                self.output.push_str("with ");
                self.comma_sep(bindings, |s, binding| {
                    if let Some(eff) = &binding.effect {
                        s.output.push_str(eff.name());
                        s.output.push_str(" => ");
                    }
                    s.unparse_expr(&binding.handler);
                });
                self.output.push_str(" do ");
                self.unparse_block(body);
            }
            NirExprKind::Resume { value } => {
                self.output.push_str("resume ");
                self.unparse_expr(value);
            }
        }
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent_level {
            self.output.push_str("    ");
        }
    }

    /// Emit the closure literal form `|name: Type, ...| body` (with an
    /// optional ` captures[...]` clause) into the unparser's output.
    /// Shared by the `NirExprKind::Closure` arm and by
    /// [`unparse_nir_closure_source`].
    fn unparse_closure_form(
        &mut self,
        params: &[(String, TypeId)],
        captures: &[crate::nir::NirCapture],
        body: &NirExpr,
    ) {
        self.delimited("|", "|", params, |s, (name, type_id)| {
            s.output.push_str(name);
            s.output.push_str(": ");
            let ty = s.type_table.type_name(*type_id);
            s.output.push_str(&ty);
        });
        if !captures.is_empty() {
            self.output.push_str(" captures");
            self.delimited("[", "]", captures, |s, cap| s.output.push_str(&cap.name));
        }
        self.output.push(' ');
        self.unparse_expr(body);
    }
}

fn emit_tir_literal_pattern(lit: &NirLiteralPattern, output: &mut String) {
    match lit {
        NirLiteralPattern::I128(v) => output.push_str(&v.to_string()),
        NirLiteralPattern::U128(v) => output.push_str(&v.to_string()),
        NirLiteralPattern::Bool(b) => output.push_str(if *b { "true" } else { "false" }),
        NirLiteralPattern::Char(c) => {
            output.push('\'');
            output.push(*c);
            output.push('\'');
        }
        NirLiteralPattern::String(s) => {
            output.push('"');
            output.push_str(s);
            output.push('"');
        }
        NirLiteralPattern::Null => output.push_str("null"),
    }
}

/// Map a NIR inline hint to its `#[inline...]` attribute, or `None` for the
/// default (no attribute).
fn inline_hint_attr(hint: crate::nir::InlineHint) -> Option<&'static str> {
    match hint {
        crate::nir::InlineHint::Auto => None,
        crate::nir::InlineHint::Hint => Some("#[inline]"),
        crate::nir::InlineHint::Always => Some("#[inline(always)]"),
        crate::nir::InlineHint::Never => Some("#[inline(never)]"),
    }
}

fn nir_binary_op_str(op: NirBinaryOp) -> &'static str {
    match op {
        NirBinaryOp::Add => "+",
        NirBinaryOp::Sub => "-",
        NirBinaryOp::Mul => "*",
        NirBinaryOp::Div => "/",
        NirBinaryOp::Mod => "%",
        NirBinaryOp::Eq => "==",
        NirBinaryOp::NotEq => "!=",
        NirBinaryOp::Lt => "<",
        NirBinaryOp::LtEq => "<=",
        NirBinaryOp::Gt => ">",
        NirBinaryOp::GtEq => ">=",
        NirBinaryOp::And => "&&",
        NirBinaryOp::Or => "||",
        NirBinaryOp::BitAnd => "&",
        NirBinaryOp::BitOr => "|",
        NirBinaryOp::BitXor => "^",
        NirBinaryOp::Shl => "<<",
        NirBinaryOp::Shr => ">>",
        NirBinaryOp::RefEq => "ref.eq",
        NirBinaryOp::RefNotEq => "ref.ne",
    }
}

fn nir_unary_op_str(op: NirUnaryOp) -> &'static str {
    match op {
        NirUnaryOp::Neg => "-",
        NirUnaryOp::Not => "!",
        NirUnaryOp::BitNot => "~",
        NirUnaryOp::Ref => "&",
        NirUnaryOp::MutRef => "&mut ",
        NirUnaryOp::Deref => "*",
    }
}

/// Unparse a NIR closure as `|name: Type, ...| body` (or `|name: Type, ...|
/// captures[...] body` when the closure captures locals) source text.
///
/// Used by `lower::closure` to bake the per-literal source string into
/// `__Closure_N^InspectAlt::inspect_alt` without requiring every NIR
/// `Closure` node to carry an unparsed-AST string.
///
/// The `captures[name1, name2, ...]` clause has no surface-syntax
/// counterpart in Wado (closures capture implicitly), but is shown in
/// the `:#?` debug output by design: it makes captured-environment
/// dependencies visible at inspect time, which is the whole point of
/// pretty-printing a closure in the first place. Non-capturing closures
/// produce output that round-trips through the parser; capturing
/// closures intentionally do not.
pub fn unparse_nir_closure_source(
    params: &[(String, TypeId)],
    captures: &[crate::nir::NirCapture],
    body: &NirExpr,
    type_table: &TypeTable,
) -> String {
    // Synthetic closures generated by `FuncRefToClosureRewriter` for a bare
    // function reference `&foo` are zero-capture wrappers whose body is
    // exactly `foo(p_0, p_1, ...)`. Render those as `&foo` instead of the
    // forwarder body so debug output reflects the user-written expression
    // rather than the lowering-internal `__fn_ref_p<i>` scaffolding.
    if let Some(rendered) = render_func_ref_closure(params, captures, body) {
        return rendered;
    }
    let mut unparser = NirUnparser::new(type_table).source_form();
    unparser.unparse_closure_form(params, captures, body);
    unparser.output
}

/// If the closure was generated by `FuncRefToClosureRewriter` for a
/// bare `&fn_name` expression, return its source-form rendering
/// (`&fn_name`); otherwise return `None`. The rewriter stamps each
/// synthetic parameter with the well-known `__fn_ref_p<i>` name, so
/// the parameter-name pattern reliably distinguishes a synthetic
/// forwarder from a user-written closure that happens to forward all
/// of its parameters to a single function (`|x: i32| double(x)` is
/// shape-identical but its parameter is named `x`, not `__fn_ref_p0`).
fn render_func_ref_closure(
    params: &[(String, TypeId)],
    captures: &[crate::nir::NirCapture],
    body: &NirExpr,
) -> Option<String> {
    if !captures.is_empty() {
        return None;
    }
    for (i, (name, _)) in params.iter().enumerate() {
        if name != &format!("__fn_ref_p{i}") {
            return None;
        }
    }
    let NirExprKind::Call { func, args, .. } = &body.kind else {
        return None;
    };
    if args.len() != params.len() {
        return None;
    }
    for (i, arg) in args.iter().enumerate() {
        let NirExprKind::Local { index, .. } = &arg.expr.kind else {
            return None;
        };
        if *index != u32::try_from(i).ok()? {
            return None;
        }
    }
    Some(format!("&{}", func.name))
}

/// Public function to unparse NIR module to pseudo-Wado source
pub fn unparse_nir(module: &NirModule) -> String {
    let type_table_ref = module.type_table.borrow();
    let unparser = NirUnparser::new(&type_table_ref);
    unparser.unparse(module)
}

/// Unparse a `NirPackage` (flat NIR lists) to pseudo-Wado source
pub fn unparse_nir_package(package: &crate::nir_package::NirPackage) -> String {
    let type_table_ref = package.type_table.borrow();
    let mut unparser = NirUnparser::new(&type_table_ref);

    // Imports
    if !package.imports.is_empty() {
        unparser.output.push_str("// Imports\n");
        for import in &package.imports {
            unparser.output.push_str("// ");
            unparser.output.push_str(&import.namespace);
            unparser.output.push_str("::");
            unparser.output.push_str(&import.canonical_name);
            unparser.output.push('\n');
        }
        unparser.output.push('\n');
    }

    // Globals
    for g in &package.globals {
        unparser.unparse_nir_global(g);
        unparser.output.push('\n');
    }

    // Structs
    for s in &package.structs {
        unparser.unparse_struct(s);
        unparser.output.push('\n');
    }

    // Enums
    for e in &package.enums {
        unparser.unparse_enum(e);
        unparser.output.push('\n');
    }

    // Flags
    for f in &package.flags {
        unparser.unparse_flags_tir(f);
        unparser.output.push('\n');
    }

    // Functions
    for f_rc in &package.functions {
        let f = f_rc.borrow();
        unparser.unparse_function(&f);
        unparser.output.push('\n');
    }

    unparser.output
}
