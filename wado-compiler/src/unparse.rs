// Unparser for Wado AST
//
// Converts AST back to canonical source code with comments.

use crate::ast::{
    AssertStmt, AssignExpr, Attribute, BinaryExpr, BinaryOp, Block, BreakStmt, CallExpr, CastExpr,
    ClosureExpr, ComparisonChainExpr, CompoundAssignExpr, CompoundAssignOp, Condition, EffectDecl,
    EffectMethod, EnumCase, EnumDecl, Expr, ExprStmt, FieldAccessExpr, ForOfStmt, ForStmt,
    Function, FunctionType, GlobalDecl, IfExpr, IfStmt, ImplBlock, ImportAttributes, IndexExpr,
    Item, LabeledBlockStmt, LetStmt, Literal, LoopStmt, MatchArm, MatchExpr, MethodCallExpr,
    Module, Newtype, Param, Pattern, ResourceDecl, ReturnStmt, SelfKind, StaticMethodCallExpr,
    Stmt, StructDecl, StructField, StructLiteralExpr, TemplateStringExpr, TestDecl, TraitDecl,
    TupleLiteralExpr, Type, UnaryExpr, UnaryOp, UseDecl, UseItem, UseItemSimple, VariantCase,
    VariantDecl, WhileStmt, WorldDecl,
};
use crate::comment::{Comment, CommentKind, CommentMap};
use crate::token::Span;
use std::collections::HashSet;

pub struct Unparser<'a> {
    comments: &'a CommentMap,
    output: String,
    indent_level: usize,
    emitted_comments: HashSet<usize>,
    last_source_line: usize,
}

impl<'a> Unparser<'a> {
    pub fn new(comments: &'a CommentMap) -> Self {
        Self {
            comments,
            output: String::new(),
            indent_level: 0,
            emitted_comments: HashSet::new(),
            last_source_line: 0,
        }
    }

    /// Emit blank lines to reach the target line, updating `last_source_line`
    fn emit_blank_lines_to(&mut self, target_line: usize) {
        if self.last_source_line > 0 && target_line > self.last_source_line {
            let blanks = self
                .comments
                .blank_lines_between(self.last_source_line, target_line);
            for _ in 0..blanks {
                self.output.push('\n');
            }
        }
        self.last_source_line = target_line;
    }

    pub fn unparse(mut self, module: &Module) -> String {
        // Output shebang if present
        if let Some(shebang) = module.shebang() {
            self.output.push_str(shebang);
            self.output.push('\n');
        }

        self.unparse_module(module);

        // Append data section if present
        if let Some(data) = module.data_section() {
            if !self.output.ends_with('\n') {
                self.output.push('\n');
            }
            self.output.push_str("\n__DATA__\n");
            self.output.push_str(data);
        }

        self.output
    }

    fn unparse_module(&mut self, module: &Module) {
        for item in &module.items {
            let item_span = get_item_span(item);
            self.unparse_item(item);
            self.last_source_line = item_span.end_line();
        }
    }

    fn unparse_item(&mut self, item: &Item) {
        let span = get_item_span(item);

        // Emit leading comments (handles blank lines before comments)
        self.emit_leading_comments(&span);

        // Emit blank lines before the item itself
        self.emit_blank_lines_to(span.line);

        match item {
            Item::Use(u) => self.unparse_use(u),
            Item::Function(f) => self.unparse_function(f),
            Item::Struct(s) => self.unparse_struct(s),
            Item::Enum(e) => self.unparse_enum(e),
            Item::Variant(v) => self.unparse_variant(v),
            Item::Flags(f) => self.unparse_flags(f),
            Item::Type(t) => self.unparse_newtype(t),
            Item::Impl(i) => self.unparse_impl(i),
            Item::Trait(t) => self.unparse_trait(t),
            Item::Effect(e) => self.unparse_effect(e),
            Item::Resource(r) => self.unparse_resource(r),
            Item::World(w) => self.unparse_world(w),
            Item::Test(t) => self.unparse_test(t),
            Item::Global(g) => self.unparse_global(g),
        }

        // Emit trailing comments
        self.emit_trailing_comments(&span);
    }

    fn unparse_use(&mut self, u: &UseDecl) {
        self.write_indent();

        if u.is_pub {
            self.output.push_str("pub ");
        }

        self.output.push_str("use { ");

        for (i, item) in u.items.iter().enumerate() {
            if i > 0 {
                self.output.push_str(", ");
            }
            self.unparse_use_item(item);
        }

        self.output.push_str(" } from \"");
        self.output.push_str(&u.source);
        self.output.push('"');

        if let Some(attrs) = &u.attributes {
            self.unparse_import_attributes(attrs);
        }

        self.output.push_str(";\n");
    }

    fn unparse_use_item(&mut self, item: &UseItem) {
        match item {
            UseItem::Simple { name, alias } => {
                self.output.push_str(name);
                if let Some(alias) = alias {
                    self.output.push_str(" as ");
                    self.output.push_str(alias);
                }
            }
            UseItem::EffectFunctions {
                effect_name,
                functions,
            } => {
                self.output.push_str(effect_name);
                self.output.push_str("::{ ");
                for (i, func) in functions.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_use_item_simple(func);
                }
                self.output.push_str(" }");
            }
        }
    }

    fn unparse_use_item_simple(&mut self, item: &UseItemSimple) {
        self.output.push_str(&item.name);
        if let Some(alias) = &item.alias {
            self.output.push_str(" as ");
            self.output.push_str(alias);
        }
    }

    fn unparse_import_attributes(&mut self, attrs: &ImportAttributes) {
        let mut parts = Vec::new();
        if let Some(v) = &attrs.version {
            parts.push(format!("version: \"{v}\""));
        }
        if let Some(t) = &attrs.type_hint {
            parts.push(format!("type: \"{t}\""));
        }
        if let Some(i) = &attrs.integrity {
            parts.push(format!("integrity: \"{i}\""));
        }
        if !parts.is_empty() {
            self.output.push_str(" with { ");
            self.output.push_str(&parts.join(", "));
            self.output.push_str(" }");
        }
    }

    fn unparse_function(&mut self, f: &Function) {
        self.write_indent();

        // Attributes
        for attr in &f.attrs {
            self.unparse_attribute(attr);
            self.output.push('\n');
            self.write_indent();
        }

        if f.is_pub {
            self.output.push_str("pub ");
        }

        if f.is_export {
            self.output.push_str("export ");
        }

        self.output.push_str("fn ");
        self.output.push_str(&f.name);
        self.unparse_generic_params(&f.type_params);
        self.output.push('(');

        for (i, param) in f.params.iter().enumerate() {
            if i > 0 {
                self.output.push_str(", ");
            }
            self.unparse_param(param);
        }

        self.output.push(')');

        // Return type
        if let Some(ret) = &f.return_type {
            self.output.push_str(" -> ");
            self.unparse_type(ret);
        }

        // Effects
        if !f.effects.is_empty() {
            self.output.push_str(" with ");
            self.output.push_str(&f.effects.join(", "));
        }

        // Body
        if let Some(body) = &f.body {
            self.output.push_str(" {\n");
            self.indent_level += 1;
            self.unparse_block(body);
            self.indent_level -= 1;
            self.write_indent();
            self.output.push_str("}\n");
        } else {
            self.output.push_str(";\n");
        }
    }

    fn unparse_param(&mut self, param: &Param) {
        match param.self_kind {
            SelfKind::Value => {
                self.output.push_str("self");
                return;
            }
            SelfKind::Ref => {
                self.output.push_str("&self");
                return;
            }
            SelfKind::MutRef => {
                self.output.push_str("&mut self");
                return;
            }
            SelfKind::None => {}
        }
        // Regular parameter
        self.output.push_str(&param.name);
        self.output.push_str(": ");
        self.unparse_type(&param.ty);
    }

    fn unparse_attribute(&mut self, attr: &Attribute) {
        self.output.push_str("#[");
        self.output.push_str(&attr.name);
        if !attr.args.is_empty() {
            self.output.push('(');
            for (i, arg) in attr.args.iter().enumerate() {
                if i > 0 {
                    self.output.push_str(", ");
                }
                self.output.push('"');
                self.output.push_str(arg);
                self.output.push('"');
            }
            self.output.push(')');
        }
        self.output.push(']');
    }

    fn unparse_struct(&mut self, s: &StructDecl) {
        self.write_indent();

        if s.is_pub {
            self.output.push_str("pub ");
        }

        self.output.push_str("struct ");
        self.output.push_str(&s.name);
        self.unparse_generic_params(&s.type_params);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        // Track line context for blank lines inside struct
        let saved_line = self.last_source_line;
        self.last_source_line = s.span.line;

        for field in &s.fields {
            self.emit_leading_comments(&field.span);
            self.emit_blank_lines_to(field.span.line);
            self.unparse_struct_field(field);
            self.emit_trailing_comments_inline(&field.span);
            self.output.push('\n');
            self.last_source_line = field.span.end_line();
        }
        self.indent_level -= 1;

        self.last_source_line = saved_line.max(s.span.end_line());
        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_struct_field(&mut self, field: &StructField) {
        self.write_indent();
        self.output.push_str(&field.name);
        self.output.push_str(": ");
        self.unparse_type(&field.ty);
        self.output.push(',');
    }

    /// Unparse generic type parameters: `<T, U: Ord>`
    fn unparse_generic_params(&mut self, params: &[crate::ast::GenericParam]) {
        if params.is_empty() {
            return;
        }
        self.output.push('<');
        for (i, param) in params.iter().enumerate() {
            if i > 0 {
                self.output.push_str(", ");
            }
            self.output.push_str(&param.name);
            if !param.bounds.is_empty() {
                self.output.push_str(": ");
                for (j, bound) in param.bounds.iter().enumerate() {
                    if j > 0 {
                        self.output.push_str(" + ");
                    }
                    self.output.push_str(bound);
                }
            }
            if let Some(default_type) = &param.default {
                self.output.push_str(" = ");
                self.unparse_type(default_type);
            }
        }
        self.output.push('>');
    }

    fn unparse_enum(&mut self, e: &EnumDecl) {
        self.write_indent();

        if e.is_pub {
            self.output.push_str("pub ");
        }

        self.output.push_str("enum ");
        self.output.push_str(&e.name);
        self.unparse_generic_params(&e.type_params);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        for case in &e.cases {
            self.unparse_enum_case(case);
        }
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_enum_case(&mut self, case: &EnumCase) {
        self.write_indent();
        self.output.push_str(&case.name);
        // Enum cases have no payload (unlike variant cases)
        self.output.push_str(",\n");
    }

    fn unparse_variant(&mut self, v: &VariantDecl) {
        self.write_indent();

        if v.is_pub {
            self.output.push_str("pub ");
        }

        self.output.push_str("variant ");
        self.output.push_str(&v.name);
        self.unparse_generic_params(&v.type_params);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        // Track line context for blank lines inside variant
        let saved_line = self.last_source_line;
        self.last_source_line = v.span.line;

        for case in &v.cases {
            self.emit_leading_comments(&case.span);
            self.emit_blank_lines_to(case.span.line);
            self.unparse_variant_case(case);
            self.emit_trailing_comments_inline(&case.span);
            self.output.push('\n');
            self.last_source_line = case.span.end_line();
        }
        self.indent_level -= 1;

        self.last_source_line = saved_line.max(v.span.end_line());
        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_variant_case(&mut self, case: &VariantCase) {
        self.write_indent();
        self.output.push_str(&case.name);
        if let Some(payload) = &case.payload {
            self.output.push('(');
            self.unparse_type(payload);
            self.output.push(')');
        }
        self.output.push(',');
    }

    fn unparse_flags(&mut self, f: &crate::ast::FlagsDecl) {
        self.write_indent();

        // Output attributes if any
        if let Some(attrs) = &f.attributes {
            for attr in attrs {
                self.unparse_attribute(attr);
            }
        }

        if f.is_pub {
            self.output.push_str("pub ");
        }

        self.output.push_str("flags ");
        self.output.push_str(&f.name);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        for flag in &f.flags {
            self.write_indent();
            self.output.push_str(&flag.name);
            self.output.push_str(",\n");
        }
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_newtype(&mut self, t: &Newtype) {
        self.write_indent();

        if t.is_pub {
            self.output.push_str("pub ");
        }

        self.output.push_str("type ");
        self.output.push_str(&t.name);
        self.output.push_str(" = ");
        self.unparse_type(&t.ty);
        self.output.push_str(";\n");
    }

    fn unparse_impl(&mut self, i: &ImplBlock) {
        self.write_indent();
        self.output.push_str("impl");
        self.unparse_generic_params(&i.type_params);
        self.output.push(' ');

        // Handle `impl Trait for Type` vs `impl Type`
        if let Some(trait_type) = &i.trait_type {
            self.unparse_type(trait_type);
            self.output.push_str(" for ");
        }

        self.unparse_type(&i.ty);
        self.output.push_str(" {\n");

        self.indent_level += 1;

        // Unparse associated type bindings
        for assoc in &i.associated_types {
            self.write_indent();
            self.output.push_str("type ");
            self.output.push_str(&assoc.name);
            self.output.push_str(" = ");
            self.unparse_type(&assoc.ty);
            self.output.push_str(";\n");
        }

        // Unparse associated constants
        for assoc_const in &i.constants {
            self.write_indent();
            if assoc_const.is_pub {
                self.output.push_str("pub ");
            }
            self.output.push_str("const ");
            self.output.push_str(&assoc_const.name);
            self.output.push_str(": ");
            self.unparse_type(&assoc_const.ty);
            self.output.push_str(" = ");
            self.unparse_expr(&assoc_const.value);
            self.output.push_str(";\n");
        }

        // Add blank line between declarations and methods if both present
        let has_declarations = !i.associated_types.is_empty() || !i.constants.is_empty();
        if has_declarations && !i.methods.is_empty() {
            self.output.push('\n');
        }

        // Save last_source_line for method context
        let saved_line = self.last_source_line;
        // Initialize to the opening brace line for proper blank line tracking
        self.last_source_line = i.span.line;

        for (idx, method) in i.methods.iter().enumerate() {
            // Get the effective start line (considering leading comments)
            let leading_comments = self.comments.leading_comments(&method.span);
            let effective_start = leading_comments
                .first()
                .map_or(method.span.line, |c| c.span.line);

            // Emit blank lines before the method (or its leading comments)
            // but ensure at least one blank line between methods
            if idx > 0 {
                let blank_lines = self
                    .comments
                    .blank_lines_between(self.last_source_line, effective_start)
                    .max(1);
                for _ in 0..blank_lines {
                    self.output.push('\n');
                }
            }

            // Now emit leading comments (without additional blank lines)
            for comment in leading_comments {
                if self.emitted_comments.insert(comment.span.start) {
                    self.write_indent();
                    self.emit_comment(comment);
                    self.output.push('\n');
                }
            }

            self.unparse_function(method);
            self.last_source_line = method.span.end_line();
        }

        self.last_source_line = saved_line.max(i.span.end_line());
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_trait(&mut self, t: &TraitDecl) {
        self.write_indent();

        if t.is_pub {
            self.output.push_str("pub ");
        }

        self.output.push_str("trait ");
        self.output.push_str(&t.name);
        self.unparse_generic_params(&t.type_params);
        self.output.push_str(" {\n");

        self.indent_level += 1;

        // Unparse associated type declarations
        for assoc in &t.associated_types {
            self.write_indent();
            self.output.push_str("type ");
            self.output.push_str(&assoc.name);
            self.output.push_str(";\n");
        }

        // Add blank line between associated types and methods if both present
        if !t.associated_types.is_empty() && !t.methods.is_empty() {
            self.output.push('\n');
        }

        // For trait declarations, don't add extra blank lines between method signatures
        for method in &t.methods {
            self.unparse_function(method);
        }
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_effect(&mut self, e: &EffectDecl) {
        self.write_indent();

        // Attributes
        for attr in &e.attrs {
            self.unparse_attribute(attr);
            self.output.push('\n');
            self.write_indent();
        }

        if e.is_pub {
            self.output.push_str("pub ");
        }

        self.output.push_str("effect ");
        self.output.push_str(&e.name);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        for method in &e.methods {
            self.unparse_effect_method(method);
        }
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_effect_method(&mut self, m: &EffectMethod) {
        self.write_indent();

        for attr in &m.attrs {
            self.unparse_attribute(attr);
            self.output.push('\n');
            self.write_indent();
        }

        self.output.push_str("fn ");
        self.output.push_str(&m.name);
        self.output.push('(');

        for (i, param) in m.params.iter().enumerate() {
            if i > 0 {
                self.output.push_str(", ");
            }
            self.output.push_str(&param.name);
            self.output.push_str(": ");
            self.unparse_type(&param.ty);
        }

        self.output.push(')');

        if let Some(ret) = &m.return_type {
            self.output.push_str(" -> ");
            self.unparse_type(ret);
        }

        self.output.push_str(";\n");
    }

    fn unparse_resource(&mut self, r: &ResourceDecl) {
        self.write_indent();

        for attr in &r.attrs {
            self.unparse_attribute(attr);
            self.output.push('\n');
            self.write_indent();
        }

        self.output.push_str("resource ");
        self.output.push_str(&r.name);

        if r.methods.is_empty() {
            self.output.push_str(";\n");
        } else {
            self.output.push_str(" {\n");

            self.indent_level += 1;
            for method in &r.methods {
                self.unparse_effect_method(method);
            }
            self.indent_level -= 1;

            self.write_indent();
            self.output.push_str("}\n");
        }
    }

    fn unparse_world(&mut self, w: &WorldDecl) {
        self.write_indent();
        self.output.push_str("world ");
        self.output.push_str(&w.name);
        self.output.push_str(" {\n");

        self.indent_level += 1;

        for imp in &w.imports {
            self.write_indent();
            self.output.push_str("import ");
            self.output.push_str(&imp.effect_name);
            self.output.push_str(" {\n");

            self.indent_level += 1;
            for func in &imp.functions {
                self.write_indent();
                self.output.push_str(func);
                self.output.push_str(",\n");
            }
            self.indent_level -= 1;

            self.write_indent();
            self.output.push_str("}\n");
        }

        for exp in &w.exports {
            self.write_indent();
            self.output.push_str("export ");
            if exp.is_async {
                self.output.push_str("async ");
            }
            self.output.push_str("fn ");
            self.output.push_str(&exp.name);
            self.output.push('(');
            for (i, param) in exp.params.iter().enumerate() {
                if i > 0 {
                    self.output.push_str(", ");
                }
                self.output.push_str(&param.name);
                self.output.push_str(": ");
                self.unparse_type(&param.ty);
            }
            self.output.push(')');
            if let Some(ret) = &exp.return_type {
                self.output.push_str(" -> ");
                self.unparse_type(ret);
            }
            self.output.push_str(";\n");
        }

        self.indent_level -= 1;
        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_test(&mut self, t: &TestDecl) {
        self.write_indent();
        self.output.push_str("test ");
        if let Some(name) = &t.name {
            self.output.push('"');
            self.output.push_str(name);
            self.output.push_str("\" ");
        }
        self.unparse_block_expr(&t.body);
        self.output.push('\n');
    }

    fn unparse_global(&mut self, g: &GlobalDecl) {
        self.write_indent();

        // Attributes
        for attr in &g.attributes {
            self.unparse_attribute(attr);
            self.output.push('\n');
            self.write_indent();
        }

        if g.is_pub {
            self.output.push_str("pub ");
        }
        self.output.push_str("global ");
        if g.mutable {
            self.output.push_str("mut ");
        }
        self.output.push_str(&g.name);
        self.output.push_str(": ");
        self.unparse_type(&g.ty);
        self.output.push_str(" = ");
        self.unparse_expr(&g.initializer);
        self.output.push_str(";\n");
    }

    fn unparse_type(&mut self, ty: &Type) {
        match ty {
            Type::Named(n) => self.output.push_str(&n.name),
            Type::Generic(g) => {
                self.output.push_str(&g.name);
                self.output.push('<');
                for (i, arg) in g.args.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_type(arg);
                }
                self.output.push('>');
            }
            Type::Function(f) => self.unparse_function_type(f),
            Type::Tuple(types) => {
                self.output.push('[');
                for (i, t) in types.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_type(t);
                }
                self.output.push(']');
            }
            Type::Reference(inner) => {
                self.output.push('&');
                self.unparse_type(inner);
            }
            Type::MutReference(inner) => {
                self.output.push_str("&mut ");
                self.unparse_type(inner);
            }
            Type::NamespacedGeneric(ng) => {
                self.output.push_str(&ng.namespace);
                self.output.push_str("::");
                self.output.push_str(&ng.name);
                if !ng.args.is_empty() {
                    self.output.push('<');
                    for (i, arg) in ng.args.iter().enumerate() {
                        if i > 0 {
                            self.output.push_str(", ");
                        }
                        self.unparse_type(arg);
                    }
                    self.output.push('>');
                }
            }
        }
    }

    fn unparse_function_type(&mut self, f: &FunctionType) {
        self.output.push_str("fn(");
        for (i, param) in f.params.iter().enumerate() {
            if i > 0 {
                self.output.push_str(", ");
            }
            self.unparse_type(param);
        }
        self.output.push(')');
        self.output.push_str(" -> ");
        self.unparse_type(&f.return_type);
        if !f.effects.is_empty() {
            self.output.push_str(" with ");
            self.output.push_str(&f.effects.join(", "));
        }
    }

    fn unparse_block(&mut self, block: &Block) {
        // Save and reset last_source_line for this block context
        let saved_line = self.last_source_line;
        self.last_source_line = block.span.line;

        for stmt in &block.stmts {
            let stmt_span = get_stmt_span(stmt);

            // Emit leading comments (handles blank lines before comments)
            self.emit_leading_comments(&stmt_span);

            // Emit blank lines before the statement itself
            self.emit_blank_lines_to(stmt_span.line);

            self.unparse_stmt(stmt);
            self.emit_trailing_comments(&stmt_span);
            self.last_source_line = stmt_span.end_line();
        }

        // Emit trailing comments in the block (between last statement and closing brace)
        self.emit_dangling_comments_in_block(block);

        // Restore for parent context
        self.last_source_line = saved_line.max(block.span.end_line());
    }

    fn unparse_stmt(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(l) => self.unparse_let(l),
            Stmt::Expr(e) => self.unparse_expr_stmt(e),
            Stmt::Return(r) => self.unparse_return(r),
            Stmt::If(i) => self.unparse_if_stmt(i),
            Stmt::While(w) => self.unparse_while(w),
            Stmt::For(f) => self.unparse_for(f),
            Stmt::ForOf(f) => self.unparse_for_of(f),
            Stmt::Loop(l) => self.unparse_loop(l),
            Stmt::Break(b) => self.unparse_break(b),
            Stmt::Continue(_) => self.unparse_continue(),
            Stmt::Assert(a) => self.unparse_assert(a),
            Stmt::LabeledBlock(lb) => self.unparse_labeled_block(lb),
        }
    }

    fn unparse_labeled_block(&mut self, lb: &LabeledBlockStmt) {
        self.write_indent();
        self.output.push_str(&lb.label);
        self.output.push_str(": {\n");

        self.indent_level += 1;
        self.unparse_block(&lb.block);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_let(&mut self, l: &LetStmt) {
        self.write_indent();
        self.output.push_str("let ");

        if l.is_reactive {
            self.output.push_str("reactive ");
        }
        if l.is_mut {
            self.output.push_str("mut ");
        }

        self.unparse_let_pattern(&l.pattern);

        if let Some(ty) = &l.ty {
            self.output.push_str(": ");
            self.unparse_type(ty);
        }

        self.output.push_str(" = ");
        self.unparse_expr(&l.value);
        self.output.push_str(";\n");
    }

    fn unparse_expr_stmt(&mut self, e: &ExprStmt) {
        self.write_indent();
        self.unparse_expr(&e.expr);
        self.output.push_str(";\n");
    }

    fn unparse_return(&mut self, r: &ReturnStmt) {
        self.write_indent();
        self.output.push_str("return");
        if let Some(value) = &r.value {
            self.output.push(' ');
            self.unparse_expr(value);
        }
        self.output.push_str(";\n");
    }

    fn unparse_if_stmt(&mut self, i: &IfStmt) {
        self.write_indent();
        self.output.push_str("if ");

        // Handle optional init binding
        if let Some(init) = &i.init {
            self.output.push_str("let ");
            if init.is_mut {
                self.output.push_str("mut ");
            }
            self.unparse_let_pattern(&init.pattern);
            if let Some(ty) = &init.ty {
                self.output.push_str(": ");
                self.unparse_type(ty);
            }
            self.output.push_str(" = ");
            self.unparse_expr(&init.value);
            self.output.push_str("; ");
        }

        self.unparse_condition(&i.condition);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&i.then_block);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push('}');

        if let Some(else_block) = &i.else_block {
            // Check if this is an `else if` (else block contains only an if statement)
            if else_block.stmts.len() == 1
                && let Stmt::If(nested_if) = &else_block.stmts[0]
            {
                self.output.push_str(" else ");
                self.unparse_if_stmt_continuation(nested_if);
                return;
            }
            self.output.push_str(" else {\n");
            self.indent_level += 1;
            self.unparse_block(else_block);
            self.indent_level -= 1;
            self.write_indent();
            self.output.push('}');
        }

        self.output.push('\n');
    }

    /// Unparse an if statement continuation (for else-if chains).
    /// This skips the initial indent and final newline since they're handled by the parent.
    fn unparse_if_stmt_continuation(&mut self, i: &IfStmt) {
        self.output.push_str("if ");

        // Handle optional init binding
        if let Some(init) = &i.init {
            self.output.push_str("let ");
            if init.is_mut {
                self.output.push_str("mut ");
            }
            self.unparse_let_pattern(&init.pattern);
            if let Some(ty) = &init.ty {
                self.output.push_str(": ");
                self.unparse_type(ty);
            }
            self.output.push_str(" = ");
            self.unparse_expr(&init.value);
            self.output.push_str("; ");
        }

        self.unparse_condition(&i.condition);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&i.then_block);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push('}');

        if let Some(else_block) = &i.else_block {
            // Check if this is an `else if` (else block contains only an if statement)
            if else_block.stmts.len() == 1
                && let Stmt::If(nested_if) = &else_block.stmts[0]
            {
                self.output.push_str(" else ");
                self.unparse_if_stmt_continuation(nested_if);
                return;
            }
            self.output.push_str(" else {\n");
            self.indent_level += 1;
            self.unparse_block(else_block);
            self.indent_level -= 1;
            self.write_indent();
            self.output.push('}');
        }

        self.output.push('\n');
    }

    fn unparse_condition(&mut self, cond: &Condition) {
        match cond {
            Condition::Expr(expr) => {
                self.unparse_expr(expr);
            }
            Condition::Pattern { pattern, expr, .. } => {
                self.output.push_str("let ");
                self.unparse_pattern(pattern);
                self.output.push_str(" = ");
                self.unparse_expr(expr);
            }
        }
    }

    fn unparse_while(&mut self, w: &WhileStmt) {
        self.write_indent();
        self.output.push_str("while ");
        self.unparse_condition(&w.condition);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&w.body);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_for(&mut self, f: &ForStmt) {
        self.write_indent();
        self.output.push_str("for ");

        if let Some(init) = &f.init {
            self.unparse_for_init(init);
        }
        self.output.push_str("; ");

        if let Some(cond) = &f.condition {
            self.unparse_condition(cond);
        }
        self.output.push_str("; ");

        if let Some(update) = &f.update {
            self.unparse_expr(update);
        }

        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&f.body);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_for_of(&mut self, f: &ForOfStmt) {
        self.write_indent();
        self.output.push_str("for let ");

        if f.is_mut {
            self.output.push_str("mut ");
        }

        self.output.push_str(&f.binding);
        self.output.push_str(" of ");
        self.unparse_expr(&f.iterable);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&f.body);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_for_init(&mut self, stmt: &Stmt) {
        match stmt {
            Stmt::Let(l) => {
                self.output.push_str("let ");
                if l.is_mut {
                    self.output.push_str("mut ");
                }
                self.unparse_let_pattern(&l.pattern);
                if let Some(ty) = &l.ty {
                    self.output.push_str(": ");
                    self.unparse_type(ty);
                }
                self.output.push_str(" = ");
                self.unparse_expr(&l.value);
            }
            Stmt::Expr(e) => {
                self.unparse_expr(&e.expr);
            }
            _ => {}
        }
    }

    fn unparse_loop(&mut self, l: &LoopStmt) {
        self.write_indent();
        self.output.push_str("loop {\n");

        self.indent_level += 1;
        self.unparse_block(&l.body);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_break(&mut self, b: &BreakStmt) {
        self.write_indent();
        self.output.push_str("break");
        if let Some(label) = &b.label {
            self.output.push(' ');
            self.output.push_str(label);
            if let Some(value) = &b.value {
                self.output.push_str(": ");
                self.unparse_expr(value);
            }
        }
        self.output.push_str(";\n");
    }

    fn unparse_continue(&mut self) {
        self.write_indent();
        self.output.push_str("continue;\n");
    }

    fn unparse_assert(&mut self, a: &AssertStmt) {
        self.write_indent();
        self.output.push_str("assert ");
        self.unparse_expr(&a.condition);
        if let Some(msg) = &a.message {
            self.output.push_str(", ");
            self.unparse_expr(msg);
        }
        self.output.push_str(";\n");
    }

    fn unparse_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Ident(i) => self.output.push_str(&i.name),
            Expr::Literal(l) => self.unparse_literal(&l.value),
            Expr::Binary(b) => self.unparse_binary(b),
            Expr::Unary(u) => self.unparse_unary(u),
            Expr::Assign(a) => self.unparse_assign(a),
            Expr::CompoundAssign(ca) => self.unparse_compound_assign(ca),
            Expr::ComparisonChain(chain) => self.unparse_comparison_chain(chain),
            Expr::Call(c) => self.unparse_call(c),
            Expr::MethodCall(m) => self.unparse_method_call(m),
            Expr::StaticMethodCall(s) => self.unparse_static_method_call(s),
            Expr::FieldAccess(f) => self.unparse_field_access(f),
            Expr::Index(i) => self.unparse_index(i),
            Expr::Block(b) => self.unparse_block_expr(b),
            Expr::If(i) => self.unparse_if_expr(i),
            Expr::Match(m) => self.unparse_match(m),
            Expr::Matches(m) => self.unparse_matches(m),
            Expr::Closure(c) => self.unparse_closure(c),
            Expr::TemplateString(t) => self.unparse_template_string(t),
            Expr::Cast(c) => self.unparse_cast(c),
            Expr::StructLiteral(s) => self.unparse_struct_literal(s),
            Expr::TupleLiteral(t) => self.unparse_tuple_literal(t),
            Expr::LabeledBlock(lb) => self.unparse_labeled_block_expr(lb),
        }
    }

    fn unparse_matches(&mut self, m: &crate::ast::MatchesExpr) {
        self.unparse_expr(&m.expr);
        self.output.push_str(" matches { ");
        self.unparse_pattern(&m.pattern);
        if let Some(guard) = &m.guard {
            self.output.push_str(" && ");
            self.unparse_expr(guard);
        }
        self.output.push_str(" }");
    }

    fn unparse_labeled_block_expr(&mut self, lb: &crate::ast::LabeledBlockExpr) {
        self.output.push_str(&lb.label);
        self.output.push_str(": {\n");
        self.indent_level += 1;
        for stmt in &lb.block.stmts {
            self.unparse_stmt(stmt);
        }
        self.indent_level -= 1;
        self.write_indent();
        self.output.push('}');
    }

    fn unparse_tuple_literal(&mut self, tuple_lit: &TupleLiteralExpr) {
        self.output.push('[');
        for (i, elem) in tuple_lit.elements.iter().enumerate() {
            if i > 0 {
                self.output.push_str(", ");
            }
            self.unparse_expr(elem);
        }
        self.output.push(']');
    }

    fn unparse_literal(&mut self, lit: &Literal) {
        match lit {
            Literal::Number(num_lit) => self.output.push_str(&num_lit.repr),
            Literal::String(s) => {
                // Use raw form to preserve multiline strings as-is
                self.output.push('"');
                self.output.push_str(&s.raw);
                self.output.push('"');
            }
            Literal::Char(c) => {
                self.output.push('\'');
                self.output.push_str(&escape_char(*c));
                self.output.push('\'');
            }
            Literal::Bool(b) => self.output.push_str(if *b { "true" } else { "false" }),
            Literal::Null => self.output.push_str("null"),
            Literal::Unit => self.output.push_str("()"),
            Literal::LocationFile => self.output.push_str("#file"),
            Literal::LocationLine => self.output.push_str("#line"),
            Literal::LocationFunction => self.output.push_str("#function"),
        }
    }

    fn unparse_binary(&mut self, b: &BinaryExpr) {
        let needs_parens_left = needs_parens(&b.left, b.op, true);
        let needs_parens_right = needs_parens(&b.right, b.op, false);

        if needs_parens_left {
            self.output.push('(');
        }
        self.unparse_expr(&b.left);
        if needs_parens_left {
            self.output.push(')');
        }

        self.output.push(' ');
        self.output.push_str(binary_op_str(b.op));
        self.output.push(' ');

        if needs_parens_right {
            self.output.push('(');
        }
        self.unparse_expr(&b.right);
        if needs_parens_right {
            self.output.push(')');
        }
    }

    fn unparse_unary(&mut self, u: &UnaryExpr) {
        self.output.push_str(unary_op_str(u.op));

        // Space between consecutive same operators: "- -5" not "--5", "& &x" not "&&x"
        let needs_space = matches!(
            (&u.op, &u.expr),
            (UnaryOp::Neg, Expr::Unary(inner)) if inner.op == UnaryOp::Neg
        ) || matches!(
            (&u.op, &u.expr),
            (UnaryOp::Ref, Expr::Unary(inner)) if inner.op == UnaryOp::Ref
        );
        if needs_space {
            self.output.push(' ');
        }

        let needs_parens = matches!(
            &u.expr,
            Expr::Binary(_) | Expr::Assign(_) | Expr::CompoundAssign(_)
        );
        if needs_parens {
            self.output.push('(');
        }
        self.unparse_expr(&u.expr);
        if needs_parens {
            self.output.push(')');
        }
    }

    fn unparse_assign(&mut self, a: &AssignExpr) {
        self.unparse_expr(&a.target);
        self.output.push_str(" = ");
        self.unparse_expr(&a.value);
    }

    fn unparse_compound_assign(&mut self, ca: &CompoundAssignExpr) {
        self.unparse_expr(&ca.target);
        self.output.push(' ');
        self.output.push_str(compound_op_str(ca.op));
        self.output.push(' ');
        self.unparse_expr(&ca.value);
    }

    fn unparse_comparison_chain(&mut self, chain: &ComparisonChainExpr) {
        self.unparse_expr(&chain.first);
        for cmp in &chain.comparisons {
            self.output.push(' ');
            self.output.push_str(binary_op_str(cmp.op));
            self.output.push(' ');
            self.unparse_expr(&cmp.right);
        }
    }

    fn unparse_call(&mut self, c: &CallExpr) {
        self.unparse_expr(&c.callee);
        // Output turbofish syntax if there are type arguments
        if !c.type_args.is_empty() {
            self.output.push_str("::<");
            for (i, ty) in c.type_args.iter().enumerate() {
                if i > 0 {
                    self.output.push_str(", ");
                }
                self.unparse_type(ty);
            }
            self.output.push('>');
        }
        self.unparse_call_args(&c.args, c.has_trailing_comma);
    }

    fn unparse_method_call(&mut self, m: &MethodCallExpr) {
        self.unparse_expr(&m.receiver);
        self.output.push('.');
        self.output.push_str(&m.method);
        // Output turbofish syntax if there are type arguments
        if !m.type_args.is_empty() {
            self.output.push_str("::<");
            for (i, ty) in m.type_args.iter().enumerate() {
                if i > 0 {
                    self.output.push_str(", ");
                }
                self.unparse_type(ty);
            }
            self.output.push('>');
        }
        self.unparse_call_args(&m.args, m.has_trailing_comma);
    }

    fn unparse_static_method_call(&mut self, s: &StaticMethodCallExpr) {
        // For generic types, use turbofish syntax: Name::<Args>
        match &s.target_type {
            Type::Generic(g) => {
                self.output.push_str(&g.name);
                self.output.push_str("::<");
                for (i, arg) in g.args.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_type(arg);
                }
                self.output.push('>');
            }
            _ => self.unparse_type(&s.target_type),
        }
        self.output.push_str("::");
        self.output.push_str(&s.method);
        self.unparse_call_args(&s.args, s.has_trailing_comma);
    }

    fn unparse_call_args(&mut self, args: &[Expr], has_trailing_comma: bool) {
        if has_trailing_comma && !args.is_empty() {
            // Multiline format with trailing comma
            self.output.push_str("(\n");
            self.indent_level += 1;
            for arg in args {
                self.write_indent();
                self.unparse_expr(arg);
                self.output.push_str(",\n");
            }
            self.indent_level -= 1;
            self.write_indent();
            self.output.push(')');
        } else {
            // Single-line format
            self.output.push('(');
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    self.output.push_str(", ");
                }
                self.unparse_expr(arg);
            }
            self.output.push(')');
        }
    }

    fn unparse_field_access(&mut self, f: &FieldAccessExpr) {
        self.unparse_expr(&f.expr);
        self.output.push('.');
        self.output.push_str(&f.field);
    }

    fn unparse_index(&mut self, i: &IndexExpr) {
        self.unparse_expr(&i.expr);
        self.output.push('[');
        self.unparse_expr(&i.index);
        self.output.push(']');
    }

    fn unparse_block_expr(&mut self, b: &Block) {
        self.output.push_str("{\n");
        self.indent_level += 1;
        self.unparse_block(b);
        self.indent_level -= 1;
        self.write_indent();
        self.output.push('}');
    }

    fn unparse_if_expr(&mut self, i: &IfExpr) {
        self.output.push_str("if ");

        // Handle optional init binding
        if let Some(init) = &i.init {
            self.output.push_str("let ");
            if init.is_mut {
                self.output.push_str("mut ");
            }
            self.unparse_let_pattern(&init.pattern);
            if let Some(ty) = &init.ty {
                self.output.push_str(": ");
                self.unparse_type(ty);
            }
            self.output.push_str(" = ");
            self.unparse_expr(&init.value);
            self.output.push_str("; ");
        }

        self.unparse_condition(&i.condition);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        self.unparse_block(&i.then_block);
        self.indent_level -= 1;

        self.write_indent();
        self.output.push('}');

        if let Some(else_block) = &i.else_block {
            // Check for else-if: block contains single if expression statement
            if else_block.stmts.len() == 1
                && let Stmt::Expr(ExprStmt {
                    expr: Expr::If(nested_if),
                    ..
                }) = &else_block.stmts[0]
            {
                // Output as `else if` instead of `else { if ... }`
                self.output.push_str(" else ");
                self.unparse_if_expr(nested_if);
                return;
            }
            self.output.push_str(" else {\n");
            self.indent_level += 1;
            self.unparse_block(else_block);
            self.indent_level -= 1;
            self.write_indent();
            self.output.push('}');
        }
    }

    fn unparse_match(&mut self, m: &MatchExpr) {
        self.output.push_str("match ");
        self.unparse_expr(&m.expr);
        self.output.push_str(" {\n");

        self.indent_level += 1;
        for arm in &m.arms {
            self.unparse_match_arm(arm);
        }
        self.indent_level -= 1;

        self.write_indent();
        self.output.push('}');
    }

    fn unparse_match_arm(&mut self, arm: &MatchArm) {
        self.write_indent();
        self.unparse_pattern(&arm.pattern);
        if let Some(guard) = &arm.guard {
            self.output.push_str(" && ");
            self.unparse_expr(guard);
        }
        self.output.push_str(" => ");
        self.unparse_expr(&arm.body);
        self.output.push_str(",\n");
    }

    fn unparse_pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Ident(name) => self.output.push_str(name),
            Pattern::Literal(lit) => self.unparse_literal(lit),
            Pattern::Wildcard => self.output.push('_'),
            Pattern::Tuple(patterns) => {
                self.output.push('[');
                for (i, p) in patterns.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_pattern(p);
                }
                self.output.push(']');
            }
            Pattern::Variant {
                variant_name,
                bindings,
                ..
            } => {
                self.output.push_str(variant_name);
                if !bindings.is_empty() {
                    self.output.push('(');
                    for (i, p) in bindings.iter().enumerate() {
                        if i > 0 {
                            self.output.push_str(", ");
                        }
                        self.unparse_pattern(p);
                    }
                    self.output.push(')');
                }
            }
        }
    }

    /// Unparse a pattern for let statements (uses brackets for tuples)
    fn unparse_let_pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Ident(name) => self.output.push_str(name),
            Pattern::Wildcard => self.output.push('_'),
            Pattern::Tuple(patterns) => {
                self.output.push('[');
                for (i, p) in patterns.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_let_pattern(p);
                }
                self.output.push(']');
            }
            Pattern::Literal(lit) => self.unparse_literal(lit),
            Pattern::Variant {
                variant_name,
                bindings,
                ..
            } => {
                self.output.push_str(variant_name);
                if !bindings.is_empty() {
                    self.output.push('(');
                    for (i, p) in bindings.iter().enumerate() {
                        if i > 0 {
                            self.output.push_str(", ");
                        }
                        self.unparse_let_pattern(p);
                    }
                    self.output.push(')');
                }
            }
        }
    }

    fn unparse_closure(&mut self, c: &ClosureExpr) {
        self.output.push('|');
        for (i, param) in c.params.iter().enumerate() {
            if i > 0 {
                self.output.push_str(", ");
            }
            self.output.push_str(&param.name);
            if let Some(ty) = &param.ty {
                self.output.push_str(": ");
                self.unparse_type(ty);
            }
        }
        self.output.push_str("| ");
        self.unparse_expr(&c.body);
    }

    fn unparse_template_string(&mut self, t: &TemplateStringExpr) {
        use crate::ast::TemplatePart;

        self.output.push('`');
        for part in &t.parts {
            match part {
                TemplatePart::String(s) => {
                    // Escape ` and { in template strings
                    for c in s.chars() {
                        match c {
                            '`' => self.output.push_str("\\`"),
                            '{' => self.output.push_str("{{"),
                            '}' => self.output.push_str("}}"),
                            _ => self.output.push(c),
                        }
                    }
                }
                TemplatePart::Interpolation { expr, format } => {
                    self.output.push('{');
                    self.unparse_expr(expr);
                    if let Some(fmt) = format {
                        self.output.push(':');
                        self.output.push_str(&fmt.spec);
                    }
                    self.output.push('}');
                }
            }
        }
        self.output.push('`');
    }

    fn unparse_cast(&mut self, c: &CastExpr) {
        let needs_parens = matches!(
            &c.expr,
            Expr::Binary(_) | Expr::Assign(_) | Expr::CompoundAssign(_)
        );
        if needs_parens {
            self.output.push('(');
        }
        self.unparse_expr(&c.expr);
        if needs_parens {
            self.output.push(')');
        }
        self.output.push_str(" as ");
        self.unparse_type(&c.target_type);
    }

    fn unparse_struct_literal(&mut self, s: &StructLiteralExpr) {
        // For named struct literals, emit the name first
        if let Some(name) = &s.name {
            self.output.push_str(name);
            self.output.push(' ');
        }

        if s.has_trailing_comma && !s.fields.is_empty() {
            // Multiline format with trailing comma
            self.output.push_str("{\n");
            self.indent_level += 1;
            for field in &s.fields {
                self.write_indent();
                self.output.push_str(&field.name);
                if !field.is_shorthand {
                    self.output.push_str(": ");
                    self.unparse_expr(&field.value);
                }
                self.output.push_str(",\n");
            }
            self.indent_level -= 1;
            self.write_indent();
            self.output.push('}');
        } else {
            // Single-line format without trailing comma
            self.output.push_str("{ ");
            for (i, field) in s.fields.iter().enumerate() {
                if i > 0 {
                    self.output.push_str(", ");
                }
                self.output.push_str(&field.name);
                if !field.is_shorthand {
                    self.output.push_str(": ");
                    self.unparse_expr(&field.value);
                }
            }
            self.output.push_str(" }");
        }
    }

    // Helper methods

    fn write_indent(&mut self) {
        for _ in 0..self.indent_level {
            self.output.push_str("    ");
        }
    }

    fn emit_leading_comments(&mut self, span: &Span) {
        for comment in self.comments.leading_comments(span) {
            if self.emitted_comments.insert(comment.span.start) {
                // Emit blank lines before this comment
                self.emit_blank_lines_to(comment.span.line);
                self.write_indent();
                self.emit_comment(comment);
                self.output.push('\n');
                self.last_source_line = comment.span.line;
            }
        }
    }

    fn emit_trailing_comments(&mut self, span: &Span) {
        for comment in self.comments.trailing_comments(span) {
            if self.emitted_comments.insert(comment.span.start) {
                // Insert before trailing newline if present
                if self.output.ends_with('\n') {
                    self.output.pop();
                    self.output.push_str("  ");
                    self.emit_comment(comment);
                    self.output.push('\n');
                } else {
                    self.output.push_str("  ");
                    self.emit_comment(comment);
                }
            }
        }
    }

    fn emit_trailing_comments_inline(&mut self, span: &Span) {
        for comment in self.comments.trailing_comments(span) {
            if self.emitted_comments.insert(comment.span.start) {
                self.output.push_str("  ");
                self.emit_comment(comment);
            }
        }
    }

    fn emit_dangling_comments_in_block(&mut self, block: &Block) {
        let after_pos = if let Some(last_stmt) = block.stmts.last() {
            get_stmt_span(last_stmt).end
        } else {
            block.span.start
        };

        for comment in self.comments.comments_between(after_pos, block.span.end) {
            if self.emitted_comments.insert(comment.span.start) {
                self.emit_blank_lines_to(comment.span.line);
                self.write_indent();
                self.emit_comment(comment);
                self.output.push('\n');
                self.last_source_line = comment.span.line;
            }
        }
    }

    fn emit_comment(&mut self, comment: &Comment) {
        match comment.kind {
            CommentKind::Line => {
                self.output.push_str("//");
                self.output.push_str(&comment.text);
            }
            CommentKind::Block => {
                self.output.push_str("/*");
                self.output.push_str(&comment.text);
                self.output.push_str("*/");
            }
        }
    }
}

// Helper functions

fn get_item_span(item: &Item) -> Span {
    match item {
        Item::Use(u) => u.span,
        Item::Function(f) => f.span,
        Item::Struct(s) => s.span,
        Item::Enum(e) => e.span,
        Item::Variant(v) => v.span,
        Item::Flags(f) => f.span,
        Item::Type(t) => t.span,
        Item::Impl(i) => i.span,
        Item::Trait(t) => t.span,
        Item::Effect(e) => e.span,
        Item::Resource(r) => r.span,
        Item::World(w) => w.span,
        Item::Test(t) => t.span,
        Item::Global(g) => g.span,
    }
}

fn get_stmt_span(stmt: &Stmt) -> Span {
    match stmt {
        Stmt::Let(l) => l.span,
        Stmt::Expr(e) => e.span,
        Stmt::Return(r) => r.span,
        Stmt::If(i) => i.span,
        Stmt::While(w) => w.span,
        Stmt::For(f) => f.span,
        Stmt::ForOf(f) => f.span,
        Stmt::Loop(l) => l.span,
        Stmt::Break(b) => b.span,
        Stmt::Continue(c) => c.span,
        Stmt::Assert(a) => a.span,
        Stmt::LabeledBlock(lb) => lb.span,
    }
}

fn binary_op_str(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::Add => "+",
        BinaryOp::Sub => "-",
        BinaryOp::Mul => "*",
        BinaryOp::Div => "/",
        BinaryOp::Mod => "%",
        BinaryOp::Eq => "==",
        BinaryOp::NotEq => "!=",
        BinaryOp::Lt => "<",
        BinaryOp::LtEq => "<=",
        BinaryOp::Gt => ">",
        BinaryOp::GtEq => ">=",
        BinaryOp::And => "&&",
        BinaryOp::Or => "||",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::BitXor => "^",
        BinaryOp::Shl => "<<",
        BinaryOp::Shr => ">>",
    }
}

fn compound_op_str(op: CompoundAssignOp) -> &'static str {
    match op {
        CompoundAssignOp::Add => "+=",
        CompoundAssignOp::Sub => "-=",
        CompoundAssignOp::Mul => "*=",
        CompoundAssignOp::Div => "/=",
        CompoundAssignOp::Mod => "%=",
    }
}

fn unary_op_str(op: UnaryOp) -> &'static str {
    match op {
        UnaryOp::Neg => "-",
        UnaryOp::Not => "!",
        UnaryOp::BitNot => "~",
        UnaryOp::Ref => "&",
        UnaryOp::MutRef => "&mut ",
        UnaryOp::Deref => "*",
    }
}

fn binary_op_precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Or => 2,
        BinaryOp::And => 3,
        BinaryOp::Eq
        | BinaryOp::NotEq
        | BinaryOp::Lt
        | BinaryOp::LtEq
        | BinaryOp::Gt
        | BinaryOp::GtEq => 4,
        BinaryOp::BitOr => 5,
        BinaryOp::BitXor => 6,
        BinaryOp::BitAnd => 7,
        BinaryOp::Shl | BinaryOp::Shr => 8,
        BinaryOp::Add | BinaryOp::Sub => 9,
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => 10,
    }
}

fn needs_parens(expr: &Expr, parent_op: BinaryOp, is_left: bool) -> bool {
    match expr {
        Expr::Binary(inner) => {
            let inner_prec = binary_op_precedence(inner.op);
            let parent_prec = binary_op_precedence(parent_op);

            if inner_prec < parent_prec {
                return true;
            }
            if inner_prec == parent_prec && !is_left {
                // Right-associative check for same precedence
                return true;
            }
            false
        }
        _ => false,
    }
}

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

// ============================================================================
// Simple AST Expression Unparser (for desugaring)
// ============================================================================

/// Unparse an AST expression to a string without comments.
/// Used by the desugar phase for generating error messages.
pub fn unparse_expr_simple(expr: &Expr) -> String {
    let mut output = String::new();
    unparse_expr_into(expr, &mut output, false);
    output
}

/// Unparse an expression into a string.
/// For error messages, we don't add parentheses to keep output readable.
fn unparse_expr_into(expr: &Expr, output: &mut String, _parens_for_binary: bool) {
    match expr {
        Expr::Ident(i) => output.push_str(&i.name),
        Expr::Literal(l) => unparse_literal_into(&l.value, output),
        Expr::Binary(b) => {
            // Don't add parentheses - keep output readable for error messages
            unparse_expr_into(&b.left, output, false);
            output.push(' ');
            output.push_str(binary_op_str(b.op));
            output.push(' ');
            unparse_expr_into(&b.right, output, false);
        }
        Expr::Unary(u) => {
            output.push_str(unary_op_str(u.op));
            unparse_expr_into(&u.expr, output, true);
        }
        Expr::Call(c) => {
            unparse_expr_into(&c.callee, output, true);
            output.push('(');
            for (i, arg) in c.args.iter().enumerate() {
                if i > 0 {
                    output.push_str(", ");
                }
                unparse_expr_into(arg, output, false);
            }
            output.push(')');
        }
        Expr::MethodCall(m) => {
            unparse_expr_into(&m.receiver, output, true);
            output.push('.');
            output.push_str(&m.method);
            output.push('(');
            for (i, arg) in m.args.iter().enumerate() {
                if i > 0 {
                    output.push_str(", ");
                }
                unparse_expr_into(arg, output, false);
            }
            output.push(')');
        }
        Expr::FieldAccess(f) => {
            unparse_expr_into(&f.expr, output, true);
            output.push('.');
            output.push_str(&f.field);
        }
        Expr::Index(i) => {
            unparse_expr_into(&i.expr, output, true);
            output.push('[');
            unparse_expr_into(&i.index, output, false);
            output.push(']');
        }
        Expr::Cast(c) => {
            unparse_expr_into(&c.expr, output, true);
            output.push_str(" as ");
            unparse_type_into(&c.target_type, output);
        }
        Expr::StaticMethodCall(s) => {
            unparse_type_into(&s.target_type, output);
            output.push_str("::");
            output.push_str(&s.method);
            output.push('(');
            for (i, arg) in s.args.iter().enumerate() {
                if i > 0 {
                    output.push_str(", ");
                }
                unparse_expr_into(arg, output, false);
            }
            output.push(')');
        }
        // For complex expressions, use a placeholder
        _ => output.push_str("<expr>"),
    }
}

fn unparse_type_into(ty: &Type, output: &mut String) {
    match ty {
        Type::Named(n) => output.push_str(&n.name),
        Type::Generic(g) => {
            output.push_str(&g.name);
            output.push('<');
            for (i, arg) in g.args.iter().enumerate() {
                if i > 0 {
                    output.push_str(", ");
                }
                unparse_type_into(arg, output);
            }
            output.push('>');
        }
        Type::Function(f) => {
            output.push_str("Fn(");
            for (i, param) in f.params.iter().enumerate() {
                if i > 0 {
                    output.push_str(", ");
                }
                unparse_type_into(param, output);
            }
            output.push_str(") -> ");
            unparse_type_into(&f.return_type, output);
        }
        Type::Tuple(types) => {
            output.push('[');
            for (i, t) in types.iter().enumerate() {
                if i > 0 {
                    output.push_str(", ");
                }
                unparse_type_into(t, output);
            }
            output.push(']');
        }
        Type::Reference(inner) => {
            output.push('&');
            unparse_type_into(inner, output);
        }
        Type::MutReference(inner) => {
            output.push_str("&mut ");
            unparse_type_into(inner, output);
        }
        Type::NamespacedGeneric(ng) => {
            output.push_str(&ng.namespace);
            output.push_str("::");
            output.push_str(&ng.name);
            if !ng.args.is_empty() {
                output.push('<');
                for (i, arg) in ng.args.iter().enumerate() {
                    if i > 0 {
                        output.push_str(", ");
                    }
                    unparse_type_into(arg, output);
                }
                output.push('>');
            }
        }
    }
}

fn unparse_literal_into(lit: &Literal, output: &mut String) {
    match lit {
        Literal::Number(num_lit) => output.push_str(&num_lit.repr),
        Literal::String(s) => {
            // Use raw form to preserve multiline strings as-is
            output.push('"');
            output.push_str(&s.raw);
            output.push('"');
        }
        Literal::Char(c) => {
            output.push('\'');
            output.push_str(&escape_char(*c));
            output.push('\'');
        }
        Literal::Bool(b) => output.push_str(if *b { "true" } else { "false" }),
        Literal::Null => output.push_str("null"),
        Literal::Unit => output.push_str("()"),
        Literal::LocationFile => output.push_str("#file"),
        Literal::LocationLine => output.push_str("#line"),
        Literal::LocationFunction => output.push_str("#function"),
    }
}

// ============================================================================
// TIR Unparser
// ============================================================================

use crate::lexer::is_valid_ident;
use crate::tir::{
    TirBinaryOp, TirBlock, TirEnum, TirExpr, TirExprKind, TirFunction, TirGlobal,
    TirLiteralPattern, TirModule, TirParam, TirPattern, TirStmt, TirStmtKind, TirStruct,
    TirUnaryOp, TypeTable,
};

/// Unparses TIR back to pseudo-Wado source code.
/// The output shows the code after monomorphization and lowering.
/// Note: Monomorphized names like `Box<i32>` are quoted to make the output parseable.
pub struct TirUnparser<'a> {
    type_table: &'a TypeTable,
    output: String,
    indent_level: usize,
}

impl<'a> TirUnparser<'a> {
    pub fn new(type_table: &'a TypeTable) -> Self {
        Self {
            type_table,
            output: String::new(),
            indent_level: 0,
        }
    }

    /// Quote an identifier if it contains characters that make it invalid Wado syntax.
    /// Valid Wado identifiers match /^[a-zA-Z_][a-zA-Z0-9_]*$/
    /// Names with `<`, `>`, `,` (monomorphized), `^` (trait), `::` (namespaced) need quoting.
    fn quote_if_needed(name: &str) -> String {
        if is_valid_ident(name) {
            name.to_string()
        } else {
            format!("\"{name}\"")
        }
    }

    pub fn unparse(mut self, module: &TirModule) -> String {
        self.unparse_module(module);
        self.output
    }

    fn unparse_module(&mut self, module: &TirModule) {
        // Imports
        if !module.imports.is_empty() {
            self.output.push_str("// Imports\n");
            for import in &module.imports {
                self.output.push_str("// ");
                self.output.push_str(&import.namespace);
                self.output.push_str("::");
                self.output.push_str(&import.canonical_name);
                self.output.push_str(" (");
                self.output.push_str(&import.func_name);
                self.output.push_str(")\n");
            }
            self.output.push('\n');
        }

        // Globals
        for g in &module.globals {
            self.unparse_tir_global(g);
            self.output.push('\n');
        }

        // Structs
        for s in &module.structs {
            self.unparse_struct(s);
            self.output.push('\n');
        }

        // Enums
        for e in &module.enums {
            self.unparse_enum(e);
            self.output.push('\n');
        }

        // Functions
        for f_rc in &module.functions {
            let f = f_rc.borrow();
            self.unparse_function(&f);
            self.output.push('\n');
        }

        // Data section
        if let Some(data) = &module.data_section {
            self.output.push_str("__DATA__\n");
            self.output.push_str(data);
        }
    }

    fn unparse_tir_global(&mut self, g: &TirGlobal) {
        self.write_indent();
        if g.is_pub {
            self.output.push_str("pub ");
        }
        self.output.push_str("global ");
        if g.mutable {
            self.output.push_str("mut ");
        }
        self.output.push_str(&g.name);
        self.output.push_str(": ");
        self.output.push_str(&self.type_table.type_name(g.ty));
        self.output.push_str(" = ");
        self.unparse_expr(&g.initializer);
        self.output.push_str(";\n");
    }

    fn unparse_struct(&mut self, s: &TirStruct) {
        self.write_indent();
        if s.is_pub {
            self.output.push_str("pub ");
        }
        self.output.push_str("struct ");
        self.output.push_str(&Self::quote_if_needed(&s.name));

        // Show generic params if present (for unmonomorphized structs)
        if !s.type_params.is_empty() {
            self.output.push('<');
            for (i, param) in s.type_params.iter().enumerate() {
                if i > 0 {
                    self.output.push_str(", ");
                }
                self.output.push_str(&param.name);
                if !param.bounds.is_empty() {
                    self.output.push_str(": ");
                    self.output.push_str(&param.bounds.join(" + "));
                }
                if let Some(default_type) = param.default {
                    self.output.push_str(" = ");
                    self.output
                        .push_str(&self.type_table.type_name(default_type));
                }
            }
            self.output.push('>');
        }

        self.output.push_str(" {\n");
        self.indent_level += 1;

        for field in &s.fields {
            self.write_indent();
            self.output.push_str(&field.name);
            self.output.push_str(": ");
            self.output
                .push_str(&self.type_table.type_name(field.type_id));
            self.output.push_str(",\n");
        }

        self.indent_level -= 1;
        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_enum(&mut self, e: &TirEnum) {
        self.write_indent();
        if e.is_pub {
            self.output.push_str("pub ");
        }
        self.output.push_str("enum ");
        self.output.push_str(&e.name);
        self.output.push_str(" {\n");
        self.indent_level += 1;

        for case in &e.cases {
            self.write_indent();
            self.output.push_str(&case.name);
            // Enum cases have no payload (unlike variant cases)
            self.output.push_str(",\n");
        }

        self.indent_level -= 1;
        self.write_indent();
        self.output.push_str("}\n");
    }

    fn unparse_function(&mut self, f: &TirFunction) {
        self.write_indent();
        if f.is_pub {
            self.output.push_str("pub ");
        }
        self.output.push_str("fn ");
        self.output.push_str(&Self::quote_if_needed(&f.name));

        // Generic params (for unmonomorphized functions)
        if !f.type_params.is_empty() {
            self.output.push('<');
            for (i, param) in f.type_params.iter().enumerate() {
                if i > 0 {
                    self.output.push_str(", ");
                }
                self.output.push_str(&param.name);
                if !param.bounds.is_empty() {
                    self.output.push_str(": ");
                    self.output.push_str(&param.bounds.join(" + "));
                }
                if let Some(default_type) = param.default {
                    self.output.push_str(" = ");
                    self.output
                        .push_str(&self.type_table.type_name(default_type));
                }
            }
            self.output.push('>');
        }

        self.output.push('(');
        for (i, param) in f.params.iter().enumerate() {
            if i > 0 {
                self.output.push_str(", ");
            }
            self.unparse_param(param);
        }
        self.output.push(')');

        // Return type
        if f.return_type != TypeTable::UNIT {
            self.output.push_str(" -> ");
            self.output
                .push_str(&self.type_table.type_name(f.return_type));
        }

        // Effects
        if !f.effects.is_empty() {
            self.output.push_str(" with ");
            self.output.push_str(&f.effects.join(", "));
        }

        // Body
        if let Some(body) = &f.body {
            self.output.push_str(" {\n");
            self.indent_level += 1;
            self.unparse_block(body);
            self.indent_level -= 1;
            self.write_indent();
            self.output.push_str("}\n");
        } else {
            self.output.push_str(";\n");
        }
    }

    fn unparse_param(&mut self, param: &TirParam) {
        self.output.push_str(&param.name);
        self.output.push_str(": ");
        self.output
            .push_str(&self.type_table.type_name(param.type_id));
    }

    fn unparse_block(&mut self, block: &TirBlock) {
        for stmt in &block.stmts {
            self.unparse_stmt(stmt);
        }
    }

    fn unparse_stmt(&mut self, stmt: &TirStmt) {
        match &stmt.kind {
            TirStmtKind::Let {
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
            TirStmtKind::Expr(expr) => {
                self.write_indent();
                self.unparse_expr(expr);
                self.output.push_str(";\n");
            }
            TirStmtKind::Return { value } => {
                self.write_indent();
                self.output.push_str("return");
                if let Some(v) = value {
                    self.output.push(' ');
                    self.unparse_expr(v);
                }
                self.output.push_str(";\n");
            }
            TirStmtKind::If {
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
            TirStmtKind::Loop { body } => {
                self.write_indent();
                self.output.push_str("loop {\n");
                self.indent_level += 1;
                self.unparse_block(body);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
            }
            TirStmtKind::Break { label, value } => {
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
            TirStmtKind::Continue => {
                self.write_indent();
                self.output.push_str("continue;\n");
            }
            TirStmtKind::LabeledBlock { label, block } => {
                self.write_indent();
                self.output.push_str(label);
                self.output.push_str(": {\n");
                self.indent_level += 1;
                self.unparse_block(block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push_str("}\n");
            }
            TirStmtKind::IfPattern {
                scrutinee,
                pattern,
                then_block,
                else_block,
            } => {
                self.write_indent();
                self.output.push_str("if ");
                self.unparse_tir_pattern(pattern);
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
            TirStmtKind::LetPattern {
                pattern,
                is_mut,
                value,
            } => {
                self.write_indent();
                self.output.push_str("let ");
                if *is_mut {
                    self.output.push_str("mut ");
                }
                self.unparse_tir_pattern(pattern);
                self.output.push_str(" = ");
                self.unparse_expr(value);
                self.output.push_str(";\n");
            }
        }
    }

    fn unparse_tir_pattern(&mut self, pattern: &TirPattern) {
        match pattern {
            TirPattern::Wildcard => {
                self.output.push('_');
            }
            TirPattern::Binding { name, .. } => {
                self.output.push_str(name);
            }
            TirPattern::Literal(lit) => match lit {
                TirLiteralPattern::I128(i) => {
                    self.output.push_str(&i.to_string());
                }
                TirLiteralPattern::U128(u) => {
                    self.output.push_str(&u.to_string());
                }
                TirLiteralPattern::Bool(b) => {
                    self.output.push_str(if *b { "true" } else { "false" });
                }
                TirLiteralPattern::Char(c) => {
                    self.output.push('\'');
                    self.output.push(*c);
                    self.output.push('\'');
                }
                TirLiteralPattern::String(s) => {
                    self.output.push('"');
                    self.output.push_str(s);
                    self.output.push('"');
                }
                TirLiteralPattern::Null => {
                    self.output.push_str("null");
                }
            },
            TirPattern::Tuple(patterns) => {
                self.output.push('[');
                for (i, p) in patterns.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_tir_pattern(p);
                }
                self.output.push(']');
            }
            TirPattern::Variant {
                variant_name,
                bindings,
                ..
            } => {
                self.output.push_str(variant_name);
                if !bindings.is_empty() {
                    self.output.push('(');
                    for (i, p) in bindings.iter().enumerate() {
                        if i > 0 {
                            self.output.push_str(", ");
                        }
                        self.unparse_tir_pattern(p);
                    }
                    self.output.push(')');
                }
            }
        }
    }

    fn unparse_expr(&mut self, expr: &TirExpr) {
        match &expr.kind {
            TirExprKind::IntLiteral { repr, .. } => {
                self.output.push_str(repr);
            }
            TirExprKind::FloatLiteral { repr, .. } => {
                self.output.push_str(repr);
            }
            TirExprKind::BoolLiteral(b) => {
                self.output.push_str(if *b { "true" } else { "false" });
            }
            TirExprKind::CharLiteral(c) => {
                self.output.push('\'');
                self.output.push_str(&escape_char(*c));
                self.output.push('\'');
            }
            TirExprKind::StringLiteral(s) => {
                self.output.push('"');
                self.output.push_str(&escape_string(s));
                self.output.push('"');
            }
            TirExprKind::Null => {
                self.output.push_str("null");
            }
            TirExprKind::OptionSome { value } => {
                self.output.push_str("Option::Some(");
                self.unparse_expr(value);
                self.output.push(')');
            }
            TirExprKind::VariantConstruct {
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
            TirExprKind::EnumConstruct { case_name, .. } => {
                // Get the enum type name from the type_id
                let type_name = self.type_table.type_name(expr.type_id);
                self.output.push_str(&type_name);
                self.output.push_str("::");
                self.output.push_str(case_name);
            }
            TirExprKind::Move { expr } => {
                self.output.push_str("move ");
                self.unparse_expr(expr);
            }
            TirExprKind::Unit => {
                self.output.push_str("()");
            }
            TirExprKind::Local { name, .. } => {
                self.output.push_str(name);
            }
            TirExprKind::Global {
                name,
                module_source,
            } => {
                let module_path = module_source.to_path();
                if !module_path.is_empty() {
                    self.output.push_str(&module_path.join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(name);
            }
            TirExprKind::GlobalVarGet {
                name,
                module_source,
            } => {
                let module_path = module_source.to_path();
                if !module_path.is_empty() {
                    self.output.push_str(&module_path.join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(name);
            }
            TirExprKind::GlobalVarSet {
                name,
                module_source,
                value,
            } => {
                let module_path = module_source.to_path();
                if !module_path.is_empty() {
                    self.output.push_str(&module_path.join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(name);
                self.output.push_str(" = ");
                self.unparse_expr(value);
            }
            TirExprKind::Capture { name, index } => {
                // Display as captured variable with index for debugging
                self.output.push_str(&format!("@capture[{index}]:{name}"));
            }
            TirExprKind::Binary { left, op, right } => {
                self.output.push('(');
                self.unparse_expr(left);
                self.output.push(' ');
                self.output.push_str(tir_binary_op_str(*op));
                self.output.push(' ');
                self.unparse_expr(right);
                self.output.push(')');
            }
            TirExprKind::Unary { op, expr: inner } => {
                self.output.push_str(tir_unary_op_str(*op));
                self.unparse_expr(inner);
            }
            TirExprKind::Assign { target, value } => {
                self.unparse_expr(target);
                self.output.push_str(" = ");
                self.unparse_expr(value);
            }
            TirExprKind::Cast {
                expr: inner,
                target_type,
            } => {
                self.unparse_expr(inner);
                self.output.push_str(" as ");
                self.output
                    .push_str(&self.type_table.type_name(*target_type));
            }
            TirExprKind::Call {
                func,
                type_args,
                args,
            } => {
                let module_path = func.module_path();
                let func_name = func.name();
                if !module_path.is_empty() {
                    self.output.push_str(&module_path.join("::"));
                    self.output.push_str("::");
                }
                self.output.push_str(&Self::quote_if_needed(&func_name));
                if !type_args.is_empty() {
                    self.output.push_str("::<");
                    for (i, type_arg) in type_args.iter().enumerate() {
                        if i > 0 {
                            self.output.push_str(", ");
                        }
                        self.output.push_str(&self.type_table.type_name(*type_arg));
                    }
                    self.output.push('>');
                }
                self.output.push('(');
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_expr(arg);
                }
                self.output.push(')');
            }
            TirExprKind::EffectCall {
                effect_name,
                op_name,
                args,
                ..
            } => {
                self.output.push_str(effect_name);
                self.output.push_str("::");
                self.output.push_str(op_name);
                self.output.push('(');
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_expr(arg);
                }
                self.output.push(')');
            }
            TirExprKind::MethodCall {
                receiver,
                func,
                type_args,
                args,
            } => {
                // Show as method call with resolved method name: receiver."Type::method"(args)
                // This shows which method was resolved during type checking
                let full_name = func.name();

                // Unparse receiver - skip the reference operator if present
                // (the resolver adds &/&mut for self methods, but we want to show just the value)
                let actual_receiver = match &receiver.kind {
                    TirExprKind::Unary {
                        op: TirUnaryOp::Ref | TirUnaryOp::MutRef,
                        expr: inner,
                    } => inner.as_ref(),
                    _ => receiver.as_ref(),
                };
                self.unparse_expr(actual_receiver);
                self.output.push('.');
                // Quote the full resolved method name to show resolution
                self.output.push_str(&Self::quote_if_needed(&full_name));

                // Type arguments (turbofish syntax)
                if !type_args.is_empty() {
                    self.output.push_str("::<");
                    for (i, type_arg) in type_args.iter().enumerate() {
                        if i > 0 {
                            self.output.push_str(", ");
                        }
                        self.output.push_str(&self.type_table.type_name(*type_arg));
                    }
                    self.output.push('>');
                }

                // Arguments
                self.output.push('(');
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_expr(arg);
                }
                self.output.push(')');
            }
            TirExprKind::StaticCall { func, args } => {
                // Output the mangled function name (e.g., "Point::origin" or "Array<i32>::with_capacity")
                self.output.push_str(&Self::quote_if_needed(&func.name()));
                self.output.push('(');
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_expr(arg);
                }
                self.output.push(')');
            }
            TirExprKind::FieldAccess {
                expr: inner,
                field_name,
                ..
            } => {
                self.unparse_expr(inner);
                self.output.push('.');
                self.output.push_str(field_name);
            }
            TirExprKind::Index { expr: array, index } => {
                self.unparse_expr(array);
                self.output.push('[');
                self.unparse_expr(index);
                self.output.push(']');
            }
            TirExprKind::Block(block) => {
                self.output.push_str("{\n");
                self.indent_level += 1;
                self.unparse_block(block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
            }
            TirExprKind::If {
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
            TirExprKind::Match {
                expr: scrutinee,
                arms,
            } => {
                self.output.push_str("match ");
                self.unparse_expr(scrutinee);
                self.output.push_str(" {\n");
                self.indent_level += 1;
                for arm in arms {
                    self.write_indent();
                    self.unparse_pattern(&arm.pattern);
                    if let Some(guard) = &arm.guard {
                        self.output.push_str(" && ");
                        self.unparse_expr(guard);
                    }
                    self.output.push_str(" => ");
                    self.unparse_expr(&arm.body);
                    self.output.push_str(",\n");
                }
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
            }
            TirExprKind::StructLiteral {
                struct_name,
                fields,
                ..
            } => {
                // If expression type is a reference, show it (for functor structs)
                if matches!(
                    self.type_table.get(expr.type_id),
                    crate::tir::ResolvedType::Ref(_)
                ) {
                    self.output.push('&');
                }
                self.output.push_str(struct_name);
                self.output.push_str(" { ");
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.output.push_str(&field.name);
                    self.output.push_str(": ");
                    self.unparse_expr(&field.value);
                }
                self.output.push_str(" }");
            }
            TirExprKind::ArrayLiteral { elements } => {
                self.output.push('[');
                for (i, elem) in elements.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_expr(elem);
                }
                self.output.push(']');
            }
            TirExprKind::TupleLiteral { elements } => {
                self.output.push('[');
                for (i, elem) in elements.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_expr(elem);
                }
                self.output.push(']');
            }
            TirExprKind::Closure {
                params,
                body,
                captures,
                functor_id: _,
            } => {
                self.output.push('|');
                for (i, (name, type_id)) in params.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.output.push_str(name);
                    self.output.push_str(": ");
                    self.output.push_str(&self.type_table.type_name(*type_id));
                }
                self.output.push('|');
                // Show captures if any
                if !captures.is_empty() {
                    self.output.push_str(" captures[");
                    for (i, cap) in captures.iter().enumerate() {
                        if i > 0 {
                            self.output.push_str(", ");
                        }
                        self.output.push_str(&cap.name);
                    }
                    self.output.push(']');
                }
                self.output.push(' ');
                self.unparse_expr(body);
            }
            TirExprKind::IndirectCall { callee, args } => {
                self.unparse_expr(callee);
                self.output.push('(');
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_expr(arg);
                }
                self.output.push(')');
            }
            TirExprKind::ClosureToCanonical { functor, .. } => {
                // Just unparse the functor - the canonical wrapper is invisible
                self.unparse_expr(functor);
            }
            TirExprKind::LabeledBlock { label, block, .. } => {
                self.output.push_str(label);
                self.output.push_str(": {\n");
                self.indent_level += 1;
                self.unparse_block(block);
                self.indent_level -= 1;
                self.write_indent();
                self.output.push('}');
            }

            // Lowered pattern matching nodes
            TirExprKind::IsNotNull { expr } => {
                self.output.push_str("__is_not_null(");
                self.unparse_expr(expr);
                self.output.push(')');
            }
            TirExprKind::UnwrapOption { expr, .. } => {
                self.output.push_str("__unwrap_option(");
                self.unparse_expr(expr);
                self.output.push(')');
            }
            TirExprKind::VariantTag { expr } => {
                self.output.push_str("__variant_tag(");
                self.unparse_expr(expr);
                self.output.push(')');
            }
            TirExprKind::VariantTest {
                expr,
                case_index,
                case_name,
            } => {
                self.output.push_str("__variant_test(");
                self.unparse_expr(expr);
                self.output
                    .push_str(&format!(", case={case_index}, name={case_name})"));
            }
            TirExprKind::VariantPayload {
                expr, case_index, ..
            } => {
                self.output.push_str("__variant_payload(");
                self.unparse_expr(expr);
                self.output.push_str(&format!(", case={case_index})"));
            }
            TirExprKind::Switch {
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
        }
    }

    fn unparse_pattern(&mut self, pattern: &TirPattern) {
        match pattern {
            TirPattern::Wildcard => self.output.push('_'),
            TirPattern::Binding { name, .. } => self.output.push_str(name),
            TirPattern::Literal(lit) => {
                use crate::tir::TirLiteralPattern;
                match lit {
                    TirLiteralPattern::I128(v) => self.output.push_str(&v.to_string()),
                    TirLiteralPattern::U128(v) => self.output.push_str(&v.to_string()),
                    TirLiteralPattern::Bool(b) => {
                        self.output.push_str(if *b { "true" } else { "false" });
                    }
                    TirLiteralPattern::Char(c) => {
                        self.output.push('\'');
                        self.output.push(*c);
                        self.output.push('\'');
                    }
                    TirLiteralPattern::String(s) => {
                        self.output.push('"');
                        self.output.push_str(s);
                        self.output.push('"');
                    }
                    TirLiteralPattern::Null => self.output.push_str("null"),
                }
            }
            TirPattern::Tuple(patterns) => {
                self.output.push('(');
                for (i, p) in patterns.iter().enumerate() {
                    if i > 0 {
                        self.output.push_str(", ");
                    }
                    self.unparse_pattern(p);
                }
                self.output.push(')');
            }
            TirPattern::Variant {
                variant_name,
                bindings,
                ..
            } => {
                self.output.push_str(variant_name);
                if !bindings.is_empty() {
                    self.output.push('(');
                    for (i, b) in bindings.iter().enumerate() {
                        if i > 0 {
                            self.output.push_str(", ");
                        }
                        self.unparse_pattern(b);
                    }
                    self.output.push(')');
                }
            }
        }
    }

    fn write_indent(&mut self) {
        for _ in 0..self.indent_level {
            self.output.push_str("    ");
        }
    }
}

fn tir_binary_op_str(op: TirBinaryOp) -> &'static str {
    match op {
        TirBinaryOp::Add => "+",
        TirBinaryOp::Sub => "-",
        TirBinaryOp::Mul => "*",
        TirBinaryOp::Div => "/",
        TirBinaryOp::Mod => "%",
        TirBinaryOp::Eq => "==",
        TirBinaryOp::NotEq => "!=",
        TirBinaryOp::Lt => "<",
        TirBinaryOp::LtEq => "<=",
        TirBinaryOp::Gt => ">",
        TirBinaryOp::GtEq => ">=",
        TirBinaryOp::And => "&&",
        TirBinaryOp::Or => "||",
        TirBinaryOp::BitAnd => "&",
        TirBinaryOp::BitOr => "|",
        TirBinaryOp::BitXor => "^",
        TirBinaryOp::Shl => "<<",
        TirBinaryOp::Shr => ">>",
    }
}

fn tir_unary_op_str(op: TirUnaryOp) -> &'static str {
    match op {
        TirUnaryOp::Neg => "-",
        TirUnaryOp::Not => "!",
        TirUnaryOp::BitNot => "~",
        TirUnaryOp::Ref => "&",
        TirUnaryOp::MutRef => "&mut ",
        TirUnaryOp::Deref => "*",
    }
}

/// Public function to unparse TIR module to pseudo-Wado source
pub fn unparse_tir(module: &TirModule) -> String {
    let type_table_ref = module.type_table.borrow();
    let unparser = TirUnparser::new(&type_table_ref);
    unparser.unparse(module)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_string() {
        assert_eq!(escape_string("hello"), "hello");
        assert_eq!(escape_string("hello\nworld"), "hello\\nworld");
        assert_eq!(escape_string("say \"hi\""), "say \\\"hi\\\"");
    }

    #[test]
    fn test_binary_op_str() {
        assert_eq!(binary_op_str(BinaryOp::Add), "+");
        assert_eq!(binary_op_str(BinaryOp::Eq), "==");
        assert_eq!(binary_op_str(BinaryOp::And), "&&");
    }

    #[test]
    fn test_compound_op_str() {
        assert_eq!(compound_op_str(CompoundAssignOp::Add), "+=");
        assert_eq!(compound_op_str(CompoundAssignOp::Div), "/=");
    }
}
