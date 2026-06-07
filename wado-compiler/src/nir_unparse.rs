//! NIR → pseudo-Wado renderer for `wado dump --nir`.
//!
//! Initially a renamed copy of [`crate::unparse`]'s `TirUnparser`. See
//! `docs/wep-2026-05-11-nir.md`.

use crate::lexer::is_valid_ident;
use crate::nir::{
    NirBinaryOp, NirEnum, NirFlags, NirFunction, NirGlobal, NirLiteralPattern, NirModule, NirParam,
    NirStruct, NirUnaryOp,
};
use crate::nir_arena::{BlockId, Body, ExprId, ExprKind, PatId, PatKind, StmtId, StmtKind};
use crate::tir::TypeTable;

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

/// Unparses NIR back to pseudo-Wado source code.
/// The output shows the code after monomorphization and lowering.
/// Note: Monomorphized names like `Box<i32>` are quoted to make the output parseable.
pub struct NirUnparser<'a> {
    type_table: &'a TypeTable,
    output: String,
    indent_level: usize,
}

impl<'a> NirUnparser<'a> {
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self {
            type_table,
            output: String::new(),
            indent_level: 0,
        }
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
        self.unparse_expr(&g.initializer, g.initializer.sole_expr());
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
            let root = body.root;
            self.emit_indented_block(|this| this.unparse_block(body, root));
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

    fn unparse_block(&mut self, body: &Body, block: BlockId) {
        for i in 0..body.blocks[block].stmts.len() {
            let sid = body.blocks[block].stmts[i];
            self.unparse_stmt(body, sid);
        }
    }

    fn unparse_stmt(&mut self, body: &Body, stmt: StmtId) {
        match &body.stmts[stmt].kind {
            StmtKind::Let {
                name,
                is_mut,
                is_reactive,
                type_id,
                value,
                ..
            } => {
                let value = *value;
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
                self.unparse_expr(body, value);
                self.output.push_str(";\n");
            }
            StmtKind::Expr(expr) => {
                let expr = *expr;
                self.write_indent();
                self.unparse_expr(body, expr);
                self.output.push_str(";\n");
            }
            StmtKind::Return { value } => {
                let value = *value;
                self.write_indent();
                self.output.push_str("return");
                if let Some(v) = value {
                    self.output.push(' ');
                    self.unparse_expr(body, v);
                }
                self.output.push_str(";\n");
            }
            StmtKind::If {
                condition,
                then_block,
                else_block,
            } => {
                let (condition, then_block, else_block) = (*condition, *then_block, *else_block);
                self.write_indent();
                self.output.push_str("if ");
                self.unparse_expr(body, condition);
                self.output.push_str(" {\n");
                self.indent_level += 1;
                self.unparse_block(body, then_block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
                if let Some(else_blk) = else_block {
                    self.output.push_str(" else {\n");
                    self.indent_level += 1;
                    self.unparse_block(body, else_blk);
                    self.indent_level -= 1;
                    self.write_indent();
                    self.output.push('}');
                }
                self.output.push('\n');
            }
            StmtKind::Loop { body: loop_body } => {
                let loop_body = *loop_body;
                self.write_indent();
                self.output.push_str("loop {\n");
                self.indent_level += 1;
                self.unparse_block(body, loop_body);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
            }
            StmtKind::Break { label, value } => {
                let value = *value;
                self.write_indent();
                self.output.push_str("break");
                if let Some(lbl) = label {
                    self.output.push(' ');
                    self.output.push_str(lbl);
                    if let Some(val) = value {
                        self.output.push_str(": ");
                        self.unparse_expr(body, val);
                    }
                }
                self.output.push_str(";\n");
            }
            StmtKind::Continue => {
                self.write_indent();
                self.output.push_str("continue;\n");
            }
            StmtKind::LabeledBlock { label, block } => {
                let block = *block;
                self.write_indent();
                self.output.push_str(label);
                self.output.push_str(": {\n");
                self.indent_level += 1;
                self.unparse_block(body, block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
            }
            StmtKind::LetDestructure {
                pattern,
                is_mut,
                value,
            } => {
                let (pattern, value) = (*pattern, *value);
                self.write_indent();
                self.output.push_str("let ");
                if *is_mut {
                    self.output.push_str("mut ");
                }
                self.unparse_nir_pattern(body, pattern);
                self.output.push_str(" = ");
                self.unparse_expr(body, value);
                self.output.push_str(";\n");
            }
        }
    }

    fn unparse_nir_pattern(&mut self, body: &Body, pat: PatId) {
        match &body.pats[pat].kind {
            PatKind::Wildcard => self.output.push('_'),
            PatKind::Binding { name, .. } => self.output.push_str(name),
            PatKind::Literal(lit) => emit_tir_literal_pattern(lit, &mut self.output),
            PatKind::Tuple(patterns, has_rest) => {
                let patterns = patterns.clone();
                let has_rest = *has_rest;
                self.output.push('[');
                self.comma_sep(patterns.iter().copied(), |s, p| {
                    s.unparse_nir_pattern(body, p);
                });
                if has_rest {
                    if !patterns.is_empty() {
                        self.output.push_str(", ");
                    }
                    self.output.push_str("..");
                }
                self.output.push(']');
            }
            PatKind::Variant {
                variant_name,
                bindings,
                ..
            } => {
                self.output.push_str(variant_name);
                if !bindings.is_empty() {
                    let bindings = bindings.clone();
                    self.delimited("(", ")", bindings.iter().copied(), |s, p| {
                        s.unparse_nir_pattern(body, p);
                    });
                }
            }
            PatKind::Enum { case_name, .. } => self.output.push_str(case_name),
            PatKind::Struct { fields, .. } => {
                let fields = fields.clone();
                self.output.push_str("{ ");
                self.comma_sep(fields.iter(), |s, field| {
                    s.output.push_str(&field.field_name);
                    if !matches!(&body.pats[field.pattern].kind, PatKind::Binding { name, .. } if name == &field.field_name)
                    {
                        s.output.push_str(": ");
                        s.unparse_nir_pattern(body, field.pattern);
                    }
                });
                self.output.push_str(" }");
            }
            PatKind::Or(alternatives) => {
                let alternatives = alternatives.clone();
                self.comma_sep_with(" | ", alternatives.iter().copied(), |s, p| {
                    s.unparse_nir_pattern(body, p);
                });
            }
            PatKind::ConstantValue { expr } => {
                let expr = *expr;
                self.unparse_expr(body, expr);
            }
            PatKind::Range {
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

    fn unparse_expr(&mut self, body: &Body, id: ExprId) {
        let ty = body.exprs[id].type_id;
        match &body.exprs[id].kind {
            ExprKind::IntLiteral { repr, .. } => {
                self.output.push_str(repr);
            }
            ExprKind::FloatLiteral { repr, .. } => {
                self.output.push_str(repr);
            }
            ExprKind::BoolLiteral(b) => {
                self.output.push_str(if *b { "true" } else { "false" });
            }
            ExprKind::CharLiteral(c) => {
                self.output.push('\'');
                self.output.push_str(&escape_char(*c));
                self.output.push('\'');
            }
            ExprKind::StringLiteral(s) => {
                self.output.push('"');
                self.output.push_str(&escape_string(s));
                self.output.push('"');
            }
            ExprKind::BytesLiteral(bytes) => {
                self.output
                    .push_str(&format!("#include_bytes(/* {} bytes */)", bytes.len()));
            }
            ExprKind::Null => {
                self.output.push_str("null");
            }
            ExprKind::VariantConstruct {
                case_name, payload, ..
            } => {
                let payload = *payload;
                // Get the variant type name from the type_id
                let type_name = self.type_table.type_name(ty);
                self.output.push_str(&type_name);
                self.output.push_str("::");
                self.output.push_str(case_name);
                if let Some(payload_expr) = payload {
                    self.output.push('(');
                    self.unparse_expr(body, payload_expr);
                    self.output.push(')');
                }
            }
            ExprKind::EnumConstruct { case_name, .. } => {
                // Get the enum type name from the type_id
                let type_name = self.type_table.type_name(ty);
                self.output.push_str(&type_name);
                self.output.push_str("::");
                self.output.push_str(case_name);
            }
            ExprKind::Unit => {
                self.output.push_str("()");
            }
            ExprKind::Local { name, .. } => {
                self.output.push_str(name);
            }
            ExprKind::GlobalVarGet {
                name,
                module_source,
            } => {
                if !module_source.is_entry_point() {
                    self.output.push_str(&module_source.to_path().join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(name);
            }
            ExprKind::GlobalVarSet {
                name,
                module_source,
                value,
            } => {
                let value = *value;
                if !module_source.is_entry_point() {
                    self.output.push_str(&module_source.to_path().join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(name);
                self.output.push_str(" = ");
                self.unparse_expr(body, value);
            }
            ExprKind::Binary { left, op, right } => {
                let (left, op, right) = (*left, *op, *right);
                self.output.push('(');
                self.unparse_expr(body, left);
                self.output.push(' ');
                self.output.push_str(nir_binary_op_str(op));
                self.output.push(' ');
                self.unparse_expr(body, right);
                self.output.push(')');
            }
            ExprKind::Unary { op, expr: inner } => {
                let (op, inner) = (*op, *inner);
                self.output.push_str(nir_unary_op_str(op));
                self.unparse_expr(body, inner);
            }
            ExprKind::Assign { target, value } => {
                let (target, value) = (*target, *value);
                self.unparse_expr(body, target);
                self.output.push_str(" = ");
                self.unparse_expr(body, value);
            }
            ExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                let (inner, target_type) = (*inner, *target_type);
                self.unparse_expr(body, inner);
                self.output.push_str(" as ");
                self.output
                    .push_str(&self.type_table.type_name(target_type));
            }
            ExprKind::Call {
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
                let type_args = type_args.clone();
                let arg_ids: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
                self.output.push_str(&Self::quote_if_needed(&full_name));
                self.unparse_type_args(&type_args);
                self.delimited("(", ")", arg_ids, |s, aid| s.unparse_expr(body, aid));
            }
            ExprKind::CmRawCall { local_name, args } => {
                let local_name = local_name.clone();
                let args = args.clone();
                self.output.push_str("cm_raw_call ");
                self.output.push_str(&local_name);
                self.delimited("(", ")", args, |s, aid| s.unparse_expr(body, aid));
            }
            ExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
                ..
            } => {
                let receiver = *receiver;
                let func_name = func.name.clone();
                let type_args = type_args.clone();
                let arg_ids: Vec<ExprId> = args.iter().map(|a| a.expr).collect();
                // The elaborator wraps `self` receivers in `&`/`&mut`
                // automatically; strip that wrapper so the rendering reflects the
                // source value.
                let actual_receiver = match &body.exprs[receiver].kind {
                    ExprKind::Unary {
                        op: NirUnaryOp::Ref | NirUnaryOp::MutRef,
                        expr: inner,
                    } => *inner,
                    _ => receiver,
                };
                self.unparse_expr(body, actual_receiver);
                self.output.push('.');
                // Quote the full resolved method name (e.g. `"Type::method"`) so
                // the output captures which impl was selected.
                self.output.push_str(&Self::quote_if_needed(&func_name));
                self.unparse_type_args(&type_args);
                self.delimited("(", ")", arg_ids, |s, aid| s.unparse_expr(body, aid));
            }
            ExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => {
                let inner = *inner;
                let field_name = field_name.clone();
                self.unparse_expr(body, inner);
                self.output.push('.');
                self.output.push_str(&field_name);
            }
            ExprKind::Index { expr: array, index } => {
                let (array, index) = (*array, *index);
                self.unparse_expr(body, array);
                self.output.push('[');
                self.unparse_expr(body, index);
                self.output.push(']');
            }
            ExprKind::Block(block) => {
                let block = *block;
                self.output.push_str("{\n");
                self.indent_level += 1;
                self.unparse_block(body, block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
            }
            ExprKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let (condition, then_branch, else_branch) =
                    (*condition, *then_branch, *else_branch);
                self.output.push_str("if ");
                self.unparse_expr(body, condition);
                self.output.push_str(" {\n");
                self.indent_level += 1;
                self.unparse_block(body, then_branch);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
                if let Some(else_blk) = else_branch {
                    self.output.push_str(" else {\n");
                    self.indent_level += 1;
                    self.unparse_block(body, else_blk);
                    self.indent_level -= 1;
                    self.write_indent();
                    self.output.push('}');
                }
            }
            ExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                let scrutinee = *scrutinee;
                let arms = arms.clone();
                self.output.push_str("match ");
                self.unparse_expr(body, scrutinee);
                self.emit_indented_block(|this| {
                    for arm in &arms {
                        this.write_indent();
                        this.unparse_nir_pattern(body, arm.pattern);
                        if let Some(guard) = arm.guard {
                            this.output.push_str(" && ");
                            this.unparse_expr(body, guard);
                        }
                        this.output.push_str(" => ");
                        this.unparse_expr(body, arm.body);
                        this.output.push_str(",\n");
                    }
                });
            }
            ExprKind::StructLiteral {
                struct_name,
                fields,
                ..
            } => {
                let struct_name = struct_name.clone();
                let field_data: Vec<(String, ExprId)> =
                    fields.iter().map(|f| (f.name.clone(), f.value)).collect();
                // Functor structs are rendered as `&Name { ... }` to mirror the
                // reference type that the elaborator attached.
                if matches!(self.type_table.get(ty), crate::tir::ResolvedType::Ref(_)) {
                    self.output.push('&');
                }
                self.output.push_str(&struct_name);
                self.output.push_str(" { ");
                self.comma_sep(field_data, |s, (name, value)| {
                    s.output.push_str(&name);
                    s.output.push_str(": ");
                    s.unparse_expr(body, value);
                });
                self.output.push_str(" }");
            }
            ExprKind::TupleLiteral { elements } | ExprKind::ArrayLiteral { elements } => {
                let elements = elements.clone();
                self.delimited("[", "]", elements, |s, e| s.unparse_expr(body, e));
            }
            ExprKind::IndirectCall { callee, args } => {
                let callee = *callee;
                let args = args.clone();
                self.unparse_expr(body, callee);
                self.delimited("(", ")", args, |s, e| s.unparse_expr(body, e));
            }
            ExprKind::ClosureToCanonical { functor, .. } => {
                // Just unparse the functor - the canonical wrapper is invisible
                let functor = *functor;
                self.unparse_expr(body, functor);
            }
            ExprKind::LabeledBlock { label, block, .. } => {
                let block = *block;
                let label = label.clone();
                self.output.push_str(&label);
                self.output.push_str(": {\n");
                self.indent_level += 1;
                self.unparse_block(body, block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
            }

            // Lowered pattern matching nodes
            ExprKind::VariantTag { expr } => {
                let expr = *expr;
                self.output.push_str("__variant_tag(");
                self.unparse_expr(body, expr);
                self.output.push(')');
            }
            ExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => {
                let (expr, case_index, case_name) = (*expr, *case_index, case_name.clone());
                self.output.push_str("__variant_test(");
                self.unparse_expr(body, expr);
                self.output
                    .push_str(&format!(", case={case_index}, name={case_name})"));
            }
            ExprKind::VariantPayload {
                expr, case_index, ..
            } => {
                let (expr, case_index) = (*expr, *case_index);
                self.output.push_str("__variant_payload(");
                self.unparse_expr(body, expr);
                self.output.push_str(&format!(", case={case_index})"));
            }
            ExprKind::Switch {
                scrutinee,
                min_value,
                arms,
                default,
            } => {
                let scrutinee = *scrutinee;
                let min_value = *min_value;
                let default = *default;
                let arms = arms.clone();
                self.output.push_str("switch ");
                self.unparse_expr(body, scrutinee);
                self.output.push_str(&format!(" (base={min_value}) {{\n"));
                self.indent_level += 1;
                for (i, arm) in arms.iter().enumerate() {
                    self.write_indent();
                    self.output
                        .push_str(&format!("{} => {{\n", min_value + i as i64));
                    self.indent_level += 1;
                    self.unparse_block(body, *arm);
                    self.indent_level -= 1;
                    self.write_indent();
                    self.output.push_str("}\n");
                }
                self.write_indent();
                self.output.push_str("_ => {\n");
                self.indent_level += 1;
                self.unparse_block(body, default);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
            }
        }
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent_level {
            self.output.push_str("    ");
        }
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
