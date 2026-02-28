use serde::Serialize;

use crate::ast::{
    EnumDecl, FlagsDecl, Function, GenericParam, GlobalDecl, ImplBlock, Item, Module, Newtype,
    Param, SelfKind, StructDecl, StructField, TraitDecl, Type, VariantDecl,
};
use crate::comment::{CommentKind, CommentMap};
use crate::unparse::{get_item_span, unparse_type_into};

#[derive(Debug, Clone, Serialize)]
pub struct DocModule {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub traits: Vec<DocTrait>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub structs: Vec<DocStruct>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<DocType>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub globals: Vec<DocGlobal>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub enums: Vec<DocEnum>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<DocVariant>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<DocFlags>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<DocFunction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocTrait {
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub associated_types: Vec<String>,
    pub methods: Vec<DocFunction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocStruct {
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub fields: Vec<DocField>,
    pub has_private_fields: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<DocFunction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocField {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocFunction {
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocType {
    pub name: String,
    pub base_type: String,
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocGlobal {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub mutable: bool,
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocEnum {
    pub signature: String,
    pub cases: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocVariant {
    pub name: String,
    pub signature: String,
    pub cases: Vec<DocVariantCase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocVariantCase {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocFlags {
    pub name: String,
    pub signature: String,
    pub members: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

pub fn extract_doc(module: &Module, comments: &CommentMap, module_name: &str) -> DocModule {
    let module_doc = extract_module_doc(comments, module);

    let mut traits: Vec<DocTrait> = Vec::new();
    let mut structs: Vec<DocStruct> = Vec::new();
    let mut types: Vec<DocType> = Vec::new();
    let mut globals: Vec<DocGlobal> = Vec::new();
    let mut enums: Vec<DocEnum> = Vec::new();
    let mut variants: Vec<DocVariant> = Vec::new();
    let mut flags: Vec<DocFlags> = Vec::new();
    let mut functions: Vec<DocFunction> = Vec::new();
    let mut impls: Vec<&ImplBlock> = Vec::new();

    // First pass: collect all impl blocks
    for item in &module.items {
        if let Item::Impl(i) = item {
            impls.push(i);
        }
    }

    for item in &module.items {
        if !is_pub_or_export(item) {
            continue;
        }
        match item {
            Item::Trait(t) => traits.push(build_doc_trait(t, comments)),
            Item::Struct(s) => structs.push(build_doc_struct(s, &impls, comments)),
            Item::Type(t) => types.push(build_doc_type(t, comments)),
            Item::Global(g) => globals.push(build_doc_global(g, comments)),
            Item::Enum(e) => enums.push(build_doc_enum(e, comments)),
            Item::Variant(v) => variants.push(build_doc_variant(v, comments)),
            Item::Flags(f) => flags.push(build_doc_flags(f, comments)),
            Item::Function(f) => functions.push(build_doc_function(f, comments)),
            _ => {}
        }
    }

    DocModule {
        name: module_name.to_string(),
        doc: module_doc,
        traits,
        structs,
        types,
        globals,
        enums,
        variants,
        flags,
        functions,
    }
}

fn build_doc_trait(t: &TraitDecl, comments: &CommentMap) -> DocTrait {
    let mut sig = String::new();
    if t.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("trait ");
    sig.push_str(&t.name);
    sig.push_str(&render_generic_params(&t.type_params));

    let associated_types: Vec<String> = t.associated_types.iter().map(|a| a.name.clone()).collect();

    let methods: Vec<DocFunction> = t
        .methods
        .iter()
        .map(|m| build_doc_function(m, comments))
        .collect();

    DocTrait {
        signature: sig,
        doc: extract_doc_comment(comments, &t.span),
        associated_types,
        methods,
    }
}

fn build_doc_struct(s: &StructDecl, impls: &[&ImplBlock], comments: &CommentMap) -> DocStruct {
    let has_private_fields = s.fields.iter().any(|f| !f.is_pub);

    let fields: Vec<DocField> = s
        .fields
        .iter()
        .filter(|f| f.is_pub)
        .map(|f| DocField {
            name: f.name.clone(),
            ty: render_type(&f.ty),
            doc: extract_doc_comment(comments, &f.span),
        })
        .collect();

    let methods = collect_pub_methods_for_type(&s.name, impls)
        .into_iter()
        .map(|m| build_doc_function(m, comments))
        .collect();

    DocStruct {
        signature: render_struct_signature(s),
        doc: extract_doc_comment(comments, &s.span),
        fields,
        has_private_fields,
        methods,
    }
}

fn build_doc_type(t: &Newtype, comments: &CommentMap) -> DocType {
    let mut sig = String::new();
    if t.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("type ");
    sig.push_str(&t.name);
    sig.push_str(" = ");
    sig.push_str(&render_type(&t.ty));

    DocType {
        name: t.name.clone(),
        base_type: render_type(&t.ty),
        signature: sig,
        doc: extract_doc_comment(comments, &t.span),
    }
}

fn build_doc_global(g: &GlobalDecl, comments: &CommentMap) -> DocGlobal {
    let mut sig = String::new();
    if g.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("global ");
    if g.mutable {
        sig.push_str("mut ");
    }
    sig.push_str(&g.name);
    sig.push_str(": ");
    sig.push_str(&render_type(&g.ty));

    DocGlobal {
        name: g.name.clone(),
        ty: render_type(&g.ty),
        mutable: g.mutable,
        signature: sig,
        doc: extract_doc_comment(comments, &g.span),
    }
}

fn build_doc_enum(e: &EnumDecl, comments: &CommentMap) -> DocEnum {
    DocEnum {
        signature: render_enum_signature(e),
        cases: e.cases.iter().map(|c| c.name.clone()).collect(),
        doc: extract_doc_comment(comments, &e.span),
    }
}

fn build_doc_variant(v: &VariantDecl, comments: &CommentMap) -> DocVariant {
    let mut sig = String::new();
    if v.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("variant ");
    sig.push_str(&v.name);
    sig.push_str(&render_generic_params(&v.type_params));

    let cases: Vec<DocVariantCase> = v
        .cases
        .iter()
        .map(|c| DocVariantCase {
            name: c.name.clone(),
            payload: c.payload.as_ref().map(render_type),
            doc: extract_doc_comment(comments, &c.span),
        })
        .collect();

    DocVariant {
        name: v.name.clone(),
        signature: sig,
        cases,
        doc: extract_doc_comment(comments, &v.span),
    }
}

fn build_doc_flags(f: &FlagsDecl, comments: &CommentMap) -> DocFlags {
    let mut sig = String::new();
    if f.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("flags ");
    sig.push_str(&f.name);

    DocFlags {
        name: f.name.clone(),
        signature: sig,
        members: f.flags.iter().map(|m| m.name.clone()).collect(),
        doc: extract_doc_comment(comments, &f.span),
    }
}

fn build_doc_function(f: &Function, comments: &CommentMap) -> DocFunction {
    DocFunction {
        signature: render_fn_signature(f),
        doc: extract_doc_comment(comments, &f.span),
    }
}

/// Extract doc text from a comment.
///
/// The lexer stores comment text *after* the `//` prefix.
/// So `/// foo` → text is `/ foo`, and `//! foo` → text is `! foo`.
/// We need to strip the leading `/` or `!` and the optional space.
fn doc_text(comment: &crate::comment::Comment) -> &str {
    let text = &comment.text;
    let rest = match comment.kind {
        CommentKind::DocLine => text.strip_prefix('/').unwrap_or(text),
        CommentKind::ModuleDoc => text.strip_prefix('!').unwrap_or(text),
        _ => text,
    };
    rest.strip_prefix(' ').unwrap_or(rest)
}

fn extract_doc_comment(comments: &CommentMap, span: &crate::token::Span) -> Option<String> {
    let leading = comments.leading_comments(span);
    let item_line = span.line;
    let mut expected_line = item_line;
    let mut doc_comments: Vec<&crate::comment::Comment> = Vec::new();

    for comment in leading.iter().rev() {
        if comment.kind != CommentKind::DocLine {
            break;
        }
        if comment.span.line >= expected_line || expected_line - comment.span.line > 1 {
            break;
        }
        expected_line = comment.span.line;
        doc_comments.push(comment);
    }

    if doc_comments.is_empty() {
        return None;
    }

    doc_comments.reverse();
    let text: Vec<&str> = doc_comments.iter().map(|c| doc_text(c)).collect();
    Some(text.join("\n"))
}

fn extract_module_doc(comments: &CommentMap, module: &Module) -> Option<String> {
    let first_item_start = module
        .items
        .first()
        .map(|item| get_item_span(item).start)
        .unwrap_or(usize::MAX);

    let doc_lines: Vec<&str> = comments
        .iter()
        .take_while(|c| c.span.start < first_item_start)
        .filter(|c| c.kind == CommentKind::ModuleDoc)
        .map(doc_text)
        .collect();

    if doc_lines.is_empty() {
        None
    } else {
        Some(doc_lines.join("\n"))
    }
}

fn is_pub_or_export(item: &Item) -> bool {
    match item {
        Item::Function(f) => f.is_pub || f.is_export,
        Item::Struct(s) => s.is_pub,
        Item::Enum(e) => e.is_pub,
        Item::Variant(v) => v.is_pub,
        Item::Flags(f) => f.is_pub,
        Item::Type(t) => t.is_pub,
        Item::Trait(t) => t.is_pub,
        Item::Effect(e) => e.is_pub,
        Item::Global(g) => g.is_pub,
        Item::Resource(r) => r.is_pub,
        Item::Impl(_) => true,
        Item::Use(_) | Item::World(_) | Item::Test(_) => false,
    }
}

fn render_type(ty: &Type) -> String {
    let mut out = String::new();
    unparse_type_into(ty, &mut out);
    out
}

fn render_generic_params(params: &[GenericParam]) -> String {
    if params.is_empty() {
        return String::new();
    }
    let mut out = String::from("<");
    for (i, param) in params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&param.name);
        if !param.bounds.is_empty() {
            out.push_str(": ");
            for (j, bound) in param.bounds.iter().enumerate() {
                if j > 0 {
                    out.push_str(" + ");
                }
                out.push_str(&bound.name);
                if !bound.assoc_types.is_empty() {
                    out.push('<');
                    for (k, assoc) in bound.assoc_types.iter().enumerate() {
                        if k > 0 {
                            out.push_str(", ");
                        }
                        out.push_str(&assoc.name);
                        out.push_str(" = ");
                        unparse_type_into(&assoc.ty, &mut out);
                    }
                    out.push('>');
                }
            }
        }
    }
    out.push('>');
    out
}

fn render_param(param: &Param) -> String {
    match param.self_kind {
        SelfKind::Ref => "&self".to_string(),
        SelfKind::MutRef => "&mut self".to_string(),
        SelfKind::None => {
            format!("{}: {}", param.name, render_type(&param.ty))
        }
    }
}

fn render_fn_signature(f: &Function) -> String {
    let mut sig = String::new();
    if f.is_pub {
        sig.push_str("pub ");
    }
    if f.is_export {
        sig.push_str("export ");
    }
    if f.is_async {
        sig.push_str("async ");
    }
    sig.push_str("fn ");
    sig.push_str(&f.name);
    sig.push_str(&render_generic_params(&f.type_params));
    sig.push('(');
    for (i, param) in f.params.iter().enumerate() {
        if i > 0 {
            sig.push_str(", ");
        }
        sig.push_str(&render_param(param));
    }
    sig.push(')');
    if let Some(ret) = &f.return_type {
        sig.push_str(" -> ");
        sig.push_str(&render_type(ret));
    }
    if !f.effects.is_empty() {
        sig.push_str(" with ");
        sig.push_str(&f.effects.join(", "));
    }
    sig
}

fn render_struct_signature(s: &StructDecl) -> String {
    let mut sig = String::new();
    if s.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("struct ");
    sig.push_str(&s.name);
    sig.push_str(&render_generic_params(&s.type_params));
    sig.push_str(" { ");

    let has_private = s.fields.iter().any(|f| !f.is_pub);
    let pub_fields: Vec<&StructField> = s.fields.iter().filter(|f| f.is_pub).collect();

    for (i, field) in pub_fields.iter().enumerate() {
        if i > 0 {
            sig.push_str(", ");
        }
        sig.push_str(&field.name);
        sig.push_str(": ");
        sig.push_str(&render_type(&field.ty));
    }

    if has_private {
        if !pub_fields.is_empty() {
            sig.push_str(", ");
        }
        sig.push_str("..");
    }

    sig.push_str(" }");
    sig
}

fn render_enum_signature(e: &EnumDecl) -> String {
    let mut sig = String::new();
    if e.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("enum ");
    sig.push_str(&e.name);
    sig.push_str(&render_generic_params(&e.type_params));
    sig.push_str(" { ");
    for (i, case) in e.cases.iter().enumerate() {
        if i > 0 {
            sig.push_str(", ");
        }
        sig.push_str(&case.name);
    }
    sig.push_str(" }");
    sig
}

fn collect_pub_methods_for_type<'a>(type_name: &str, impls: &[&'a ImplBlock]) -> Vec<&'a Function> {
    let mut methods = Vec::new();
    for i in impls {
        let target_name = match &i.ty {
            Type::Named(n) => &n.name,
            Type::Generic(g) => &g.name,
            _ => continue,
        };
        if target_name != type_name {
            continue;
        }
        for m in &i.methods {
            if m.is_pub || m.is_export {
                methods.push(m);
            }
        }
    }
    methods
}
