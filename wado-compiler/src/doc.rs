use crate::hashmap::IndexSet;
use serde::Serialize;

use crate::ast::{
    AssociatedConst, AstId, Attribute, EnumDecl, FlagsDecl, Function, GenericParam, GlobalDecl,
    ImplBlock, InterfaceDecl, Item, Module, Newtype, Param, SelfKind, StructDecl, StructField,
    TraitDecl, Type, UseItem, VariantDecl,
};
use crate::comment::{CommentKind, TriviaMap};
use crate::stdlib;
use crate::token::Span;
use crate::unparse::{get_item_id, unparse_type_into};

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
    pub effects: Vec<DocEffect>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<DocResource>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<DocFunction>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub primitive_types: Vec<DocPrimitiveType>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocPrimitiveType {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub constants: Vec<DocFunction>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<DocFunction>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trait_impls: Vec<DocTraitImpl>,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trait_impls: Vec<DocTraitImpl>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocTraitImpl {
    pub signature: String,
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
    pub cases: Vec<DocEnumCase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocEnumCase {
    pub name: String,
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
pub struct DocEffect {
    pub name: String,
    pub signature: String,
    pub methods: Vec<DocFunction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocResource {
    pub name: String,
    pub signature: String,
    pub methods: Vec<DocFunction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocFlags {
    pub name: String,
    pub signature: String,
    pub members: Vec<DocFlagsMember>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocFlagsMember {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
}

pub fn extract_doc(module: &Module, trivia: &TriviaMap, module_name: &str) -> DocModule {
    let module_doc = extract_module_doc(trivia, module);

    let mut traits: Vec<DocTrait> = Vec::new();
    let mut structs: Vec<DocStruct> = Vec::new();
    let mut types: Vec<DocType> = Vec::new();
    let mut globals: Vec<DocGlobal> = Vec::new();
    let mut enums: Vec<DocEnum> = Vec::new();
    let mut variants: Vec<DocVariant> = Vec::new();
    let mut flags: Vec<DocFlags> = Vec::new();
    let mut effects: Vec<DocEffect> = Vec::new();
    let mut resources: Vec<DocResource> = Vec::new();
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
            Item::Trait(t) => traits.push(build_doc_trait(t, trivia)),
            Item::Struct(s) => structs.push(build_doc_struct(s, &impls, trivia)),
            Item::Newtype(t) => types.push(build_doc_type(t, trivia)),
            Item::Global(g) => globals.push(build_doc_global(g, trivia)),
            Item::Enum(e) => enums.push(build_doc_enum(e, trivia)),
            Item::Variant(v) => variants.push(build_doc_variant(v, trivia)),
            Item::Flags(f) => flags.push(build_doc_flags(f, trivia)),
            Item::Interface(e) => effects.push(build_doc_interface(e, trivia)),
            Item::Resource(r) => resources.push(build_doc_resource(r, trivia)),
            Item::Function(f) => functions.push(build_doc_function(f, trivia)),
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
        effects,
        resources,
        functions,
        primitive_types: Vec::new(),
    }
}

fn build_doc_trait(t: &TraitDecl, trivia: &TriviaMap) -> DocTrait {
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
        .map(|m| build_doc_function(m, trivia))
        .collect();

    DocTrait {
        signature: sig,
        // Pass `t.attrs` so the doc comment lookup bridges over any
        // `#[compiler_item("...")]` (or other) attribute that sits
        // between the `///` block and the `pub trait` keyword.
        // Without it the doc tool stops at the attribute line and the
        // generated stdlib reference loses the trait's description.
        doc: extract_doc_comment_with_attrs(trivia, t.id, &t.span, &t.attrs),
        associated_types,
        methods,
    }
}

fn build_doc_struct(s: &StructDecl, impls: &[&ImplBlock], trivia: &TriviaMap) -> DocStruct {
    // `__`-prefixed fields are an internal-naming convention (CM ABI
    // plumbing on `AsyncCall<T>`, etc.). Treat them like private fields
    // for documentation: hide the field row but still flag the struct
    // as having hidden state via `has_private_fields` so the rendered
    // signature gets a `..` placeholder.
    let is_hidden = |f: &StructField| !f.is_pub || f.name.starts_with("__");
    let has_private_fields = s.fields.iter().any(is_hidden);

    let fields: Vec<DocField> = s
        .fields
        .iter()
        .filter(|f| !is_hidden(f))
        .map(|f| DocField {
            name: f.name.clone(),
            ty: render_type(&f.ty),
            doc: extract_doc_comment_with_attrs(trivia, f.id, &f.span, &f.attrs),
        })
        .collect();

    let (methods, trait_impls) = collect_impl_methods_for_type(&s.name, impls, trivia);

    DocStruct {
        signature: render_struct_signature(s),
        doc: extract_doc_comment_with_attrs(trivia, s.id, &s.span, &s.attrs),
        fields,
        has_private_fields,
        methods,
        trait_impls,
    }
}

fn build_doc_type(t: &Newtype, trivia: &TriviaMap) -> DocType {
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
        doc: extract_doc_comment(trivia, t.id, &t.span),
    }
}

fn build_doc_global(g: &GlobalDecl, trivia: &TriviaMap) -> DocGlobal {
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
        doc: extract_doc_comment(trivia, g.id, &g.span),
    }
}

fn build_doc_enum(e: &EnumDecl, trivia: &TriviaMap) -> DocEnum {
    let cases = e
        .cases
        .iter()
        .map(|c| DocEnumCase {
            name: c.name.clone(),
            doc: extract_doc_comment_with_attrs(trivia, c.id, &c.span, &c.attrs),
        })
        .collect();
    DocEnum {
        signature: render_enum_signature(e),
        cases,
        doc: extract_doc_comment_with_attrs(trivia, e.id, &e.span, &e.attrs),
    }
}

fn build_doc_variant(v: &VariantDecl, trivia: &TriviaMap) -> DocVariant {
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
            doc: extract_doc_comment_with_attrs(trivia, c.id, &c.span, &c.attrs),
        })
        .collect();

    DocVariant {
        name: v.name.clone(),
        signature: sig,
        cases,
        doc: extract_doc_comment_with_attrs(trivia, v.id, &v.span, &v.attrs),
    }
}

fn build_doc_flags(f: &FlagsDecl, trivia: &TriviaMap) -> DocFlags {
    let mut sig = String::new();
    if f.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("flags ");
    sig.push_str(&f.name);

    let members = f
        .flags
        .iter()
        .map(|m| DocFlagsMember {
            name: m.name.clone(),
            doc: extract_doc_comment_with_attrs(trivia, m.id, &m.span, &m.attrs),
        })
        .collect();
    DocFlags {
        name: f.name.clone(),
        signature: sig,
        members,
        doc: extract_doc_comment_with_attrs(
            trivia,
            f.id,
            &f.span,
            f.attributes.as_deref().unwrap_or(&[]),
        ),
    }
}

fn build_doc_interface(e: &InterfaceDecl, trivia: &TriviaMap) -> DocEffect {
    let mut sig = String::new();
    if e.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("interface ");
    sig.push_str(&e.name);

    let methods: Vec<DocFunction> = e
        .methods
        .iter()
        .map(|m| DocFunction {
            signature: render_interface_method_signature(m),
            doc: extract_doc_comment_with_attrs(trivia, m.id, &m.span, &m.attrs),
        })
        .collect();

    DocEffect {
        name: e.name.clone(),
        signature: sig,
        methods,
        doc: extract_doc_comment_with_attrs(trivia, e.id, &e.span, &e.attrs),
    }
}

fn build_doc_resource(r: &crate::ast::ResourceDecl, trivia: &TriviaMap) -> DocResource {
    let mut sig = String::new();
    if r.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("resource ");
    sig.push_str(&r.name);
    sig.push_str(&render_generic_params(&r.type_params));

    let methods: Vec<DocFunction> = r
        .methods
        .iter()
        .map(|m| DocFunction {
            signature: render_interface_method_signature(m),
            doc: extract_doc_comment_with_attrs(trivia, m.id, &m.span, &m.attrs),
        })
        .collect();

    DocResource {
        name: r.name.clone(),
        signature: sig,
        methods,
        doc: extract_doc_comment_with_attrs(trivia, r.id, &r.span, &r.attrs),
    }
}

fn build_doc_function(f: &Function, trivia: &TriviaMap) -> DocFunction {
    DocFunction {
        signature: render_fn_signature(f),
        doc: extract_doc_comment_with_attrs(trivia, f.id, &f.span, &f.attrs),
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

fn extract_doc_comment(trivia: &TriviaMap, id: AstId, span: &Span) -> Option<String> {
    extract_doc_comment_with_attrs(trivia, id, span, &[])
}

fn extract_doc_comment_with_attrs(
    trivia: &TriviaMap,
    id: AstId,
    span: &Span,
    attrs: &[Attribute],
) -> Option<String> {
    let leading = trivia.leading_of(id);
    // When attributes are present, the doc comment is before the first attribute,
    // not immediately before the keyword. Use the first attribute's line as the
    // start to bridge the gap.
    let item_line = attrs.first().map_or(span.line, |a| a.span.line);
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

fn extract_module_doc(trivia: &TriviaMap, module: &Module) -> Option<String> {
    // `//!` module-doc comments are pinned by the parser to the leading
    // trivia of the first allocated id (= the first item, since the
    // module itself has no `AstId`). When the module has no items at
    // all, the comments end up in `dangling`.
    let leading = match module.items.first() {
        Some(first) => trivia.leading_of(get_item_id(first)),
        None => trivia.dangling(),
    };
    let doc_lines: Vec<&str> = leading
        .iter()
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
        Item::Newtype(t) => t.is_pub,
        Item::Trait(t) => t.is_pub,
        Item::Interface(e) => e.is_pub,
        Item::Global(g) => g.is_pub,
        Item::Resource(r) => r.is_pub,
        Item::Impl(_) => true,
        Item::TupleTypeDecl(d) => d.is_pub,
        Item::Use(_) | Item::World(_) | Item::Test(_) => false,
        Item::Error(_) => false,
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

fn render_interface_method_signature(m: &crate::ast::InterfaceMethod) -> String {
    let mut sig = String::new();
    if m.is_async {
        sig.push_str("async ");
    }
    sig.push_str("fn ");
    sig.push_str(&m.name);
    sig.push('(');
    for (i, param) in m.params.iter().enumerate() {
        if i > 0 {
            sig.push_str(", ");
        }
        sig.push_str(&render_param(param));
    }
    sig.push(')');
    if let Some(ret) = &m.return_type {
        sig.push_str(" -> ");
        sig.push_str(&render_type(ret));
    }
    sig
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

    // Mirror `build_doc_struct`: `__`-prefixed fields are hidden in docs.
    let is_hidden = |f: &StructField| !f.is_pub || f.name.starts_with("__");
    let has_private = s.fields.iter().any(is_hidden);
    let pub_fields: Vec<&StructField> = s.fields.iter().filter(|f| !is_hidden(f)).collect();

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

/// Resolve a stdlib module name (e.g., "core:cli", "wasi:http") to source code.
///
/// Returns `None` if the name is not a known stdlib module.
pub fn resolve_stdlib_source(module_name: &str) -> Option<&'static str> {
    stdlib::get_stdlib_module(module_name)
}

/// Parse a bundled stdlib source. Bundled stdlib must always parse cleanly;
/// a recovered lex or parse error here is a compiler bug, so fail loudly
/// rather than silently produce partial docs (matches `parse_bind_stdlib`
/// in `loader.rs`).
fn parse_stdlib_for_doc(label: &str, source: &str) -> crate::ParseResult {
    let parsed = crate::parse(source);
    assert!(
        parsed.lex_errors.is_empty(),
        "stdlib '{label}' must lex cleanly: {:?}",
        parsed.lex_errors,
    );
    assert!(
        parsed.errors.is_empty(),
        "stdlib '{label}' must parse cleanly: {:?}",
        parsed.errors,
    );
    parsed
}

/// Extract documentation from a stdlib module by name.
///
/// For `core:prelude`, follows `pub use` re-exports and merges items from
/// the sub-modules into a single `DocModule`. For other modules, extracts
/// docs directly from the source.
///
/// Returns `None` if the module name is not a known stdlib module.
pub fn extract_stdlib_doc(module_name: &str) -> Option<DocModule> {
    let source = stdlib::get_stdlib_module(module_name)?;
    let parsed = parse_stdlib_for_doc(module_name, source);
    let mut doc = extract_doc(&parsed.ast, &parsed.trivia, module_name);

    // For modules with pub use re-exports, follow them to get the actual items
    let reexport_sources = collect_pub_use_sources(&parsed.ast);
    if !reexport_sources.is_empty() {
        let exported_names = collect_pub_use_names(&parsed.ast);
        for reexport_source in &reexport_sources {
            if let Some(sub_source) = stdlib::get_stdlib_module(reexport_source) {
                let sub_parsed = parse_stdlib_for_doc(reexport_source, sub_source);
                let sub_doc = extract_doc(&sub_parsed.ast, &sub_parsed.trivia, reexport_source);
                merge_reexported_items(&mut doc, &sub_doc, &exported_names);
            }
        }
    }

    // Follow `use _ from "..."` (side-effect imports) to collect impl blocks on primitive types
    let side_effect_sources = collect_side_effect_import_sources(&parsed.ast);
    for se_source in &side_effect_sources {
        if let Some(sub_source) = stdlib::get_stdlib_module(se_source) {
            let sub_parsed = parse_stdlib_for_doc(se_source, sub_source);
            let prim_types =
                collect_primitive_types_from_module(&sub_parsed.ast, &sub_parsed.trivia);
            merge_primitive_types(&mut doc.primitive_types, prim_types);
        }
    }

    // Also collect from any pub use re-export sources that may have primitive impls
    for reexport_source in &reexport_sources {
        if let Some(sub_source) = stdlib::get_stdlib_module(reexport_source) {
            let sub_parsed = parse_stdlib_for_doc(reexport_source, sub_source);
            // Recursively follow side-effect imports from re-exported sub-modules
            let sub_se_sources = collect_side_effect_import_sources(&sub_parsed.ast);
            for sub_se in &sub_se_sources {
                if let Some(se_source) = stdlib::get_stdlib_module(sub_se) {
                    let se_parsed = parse_stdlib_for_doc(sub_se, se_source);
                    let prim_types =
                        collect_primitive_types_from_module(&se_parsed.ast, &se_parsed.trivia);
                    merge_primitive_types(&mut doc.primitive_types, prim_types);
                }
            }
            // Also check the re-exported module itself
            let prim_types =
                collect_primitive_types_from_module(&sub_parsed.ast, &sub_parsed.trivia);
            merge_primitive_types(&mut doc.primitive_types, prim_types);
        }
    }

    Some(doc)
}

/// Collect source paths from `pub use { ... } from "source"` declarations.
fn collect_pub_use_sources(module: &Module) -> Vec<String> {
    let mut sources = Vec::new();
    for item in &module.items {
        if let Item::Use(u) = item
            && u.is_pub
            && !u.items.iter().any(|i| matches!(i, UseItem::Wildcard))
        {
            sources.push(u.source.clone());
        }
    }
    sources
}

/// Collect item names from `pub use { Name1, Name2 } from "..."` declarations.
fn collect_pub_use_names(module: &Module) -> IndexSet<String> {
    let mut names = IndexSet::default();
    for item in &module.items {
        if let Item::Use(u) = item
            && u.is_pub
        {
            for use_item in &u.items {
                match use_item {
                    UseItem::Simple { name, alias, .. } => {
                        names.insert(alias.as_ref().unwrap_or(name).clone());
                    }
                    UseItem::InterfaceFunctions { interface_name, .. } => {
                        names.insert(interface_name.clone());
                    }
                    UseItem::Wildcard | UseItem::Namespace { .. } => {}
                }
            }
        }
    }
    names
}

/// Merge items from a sub-module into the parent doc, filtered by re-exported names.
fn merge_reexported_items(parent: &mut DocModule, child: &DocModule, names: &IndexSet<String>) {
    for t in &child.traits {
        if names.contains(extract_item_name(&t.signature, "trait ")) {
            parent.traits.push(t.clone());
        }
    }
    for s in &child.structs {
        if names.contains(extract_item_name(&s.signature, "struct ")) {
            parent.structs.push(s.clone());
        }
    }
    for t in &child.types {
        if names.contains(&t.name) {
            parent.types.push(t.clone());
        }
    }
    for g in &child.globals {
        if names.contains(&g.name) {
            parent.globals.push(g.clone());
        }
    }
    for e in &child.enums {
        if names.contains(extract_item_name(&e.signature, "enum ")) {
            parent.enums.push(e.clone());
        }
    }
    for v in &child.variants {
        if names.contains(&v.name) {
            parent.variants.push(v.clone());
        }
    }
    for f in &child.flags {
        if names.contains(&f.name) {
            parent.flags.push(f.clone());
        }
    }
    for e in &child.effects {
        if names.contains(&e.name) {
            parent.effects.push(e.clone());
        }
    }
    for r in &child.resources {
        if names.contains(&r.name) {
            parent.resources.push(r.clone());
        }
    }
    for f in &child.functions {
        if names.contains(extract_item_name(&f.signature, "fn ")) {
            parent.functions.push(f.clone());
        }
    }
}

/// Merge primitive impl entries, combining methods for the same type name.
fn merge_primitive_types(target: &mut Vec<DocPrimitiveType>, source: Vec<DocPrimitiveType>) {
    for src in source {
        if let Some(existing) = target.iter_mut().find(|t| t.name == src.name) {
            existing.constants.extend(src.constants);
            existing.methods.extend(src.methods);
            existing.trait_impls.extend(src.trait_impls);
        } else {
            target.push(src);
        }
    }
}

/// Extract the item name from a signature string.
/// e.g., "pub trait Eq" with keyword "trait " → "Eq"
fn extract_item_name<'a>(sig: &'a str, keyword: &str) -> &'a str {
    let rest = sig
        .find(keyword)
        .map(|i| &sig[i + keyword.len()..])
        .unwrap_or(sig);
    // Take until first non-identifier char
    let end = rest
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(rest.len());
    &rest[..end]
}

fn collect_impl_methods_for_type(
    type_name: &str,
    impls: &[&ImplBlock],
    trivia: &TriviaMap,
) -> (Vec<DocFunction>, Vec<DocTraitImpl>) {
    let mut inherent_methods = Vec::new();
    let mut trait_impls = Vec::new();

    for i in impls {
        let target_name = match &i.ty {
            Type::Named(n) => &n.name,
            Type::Generic(g) => &g.name,
            _ => continue,
        };
        if target_name != type_name {
            continue;
        }

        if let Some(ref trait_ty) = i.trait_type {
            let methods: Vec<DocFunction> = i
                .methods
                .iter()
                .map(|m| build_doc_function(m, trivia))
                .collect();
            if methods.is_empty() {
                continue;
            }
            let trait_name = render_type(trait_ty);
            let type_sig = render_type(&i.ty);
            trait_impls.push(DocTraitImpl {
                signature: format!("impl {trait_name} for {type_sig}"),
                methods,
            });
        } else {
            for m in &i.methods {
                if m.is_pub || m.is_export {
                    inherent_methods.push(build_doc_function(m, trivia));
                }
            }
        }
    }

    (inherent_methods, trait_impls)
}

const PRIMITIVE_TYPE_NAMES: &[&str] = &[
    "bool", "char", "i8", "u8", "i16", "u16", "i32", "u32", "i64", "u64", "f32", "f64",
];

fn build_doc_const(c: &AssociatedConst, trivia: &TriviaMap) -> DocFunction {
    let mut sig = String::new();
    if c.is_pub {
        sig.push_str("pub ");
    }
    sig.push_str("const ");
    sig.push_str(&c.name);
    sig.push_str(": ");
    unparse_type_into(&c.ty, &mut sig);
    DocFunction {
        signature: sig,
        doc: extract_doc_comment_with_attrs(trivia, c.id, &c.span, &[]),
    }
}

/// Collect `use _ from "..."` (side-effect import) source paths.
fn collect_side_effect_import_sources(module: &Module) -> Vec<String> {
    let mut sources = Vec::new();
    for item in &module.items {
        if let Item::Use(u) = item
            && u.items.iter().any(|i| matches!(i, UseItem::Wildcard))
        {
            sources.push(u.source.clone());
        }
    }
    sources
}

/// Collect `impl` blocks on primitive types from a parsed module, grouping by type name.
fn collect_primitive_types_from_module(
    module: &Module,
    trivia: &TriviaMap,
) -> Vec<DocPrimitiveType> {
    use crate::hashmap::IndexMap;

    let mut impls: Vec<&ImplBlock> = Vec::new();
    for item in &module.items {
        if let Item::Impl(i) = item {
            impls.push(i);
        }
    }

    let mut by_name: IndexMap<&str, DocPrimitiveType> = IndexMap::default();

    for &prim_name in PRIMITIVE_TYPE_NAMES {
        let (methods, trait_impls) = collect_impl_methods_for_type(prim_name, &impls, trivia);

        // Also collect associated constants
        let mut constants = Vec::new();
        for i in &impls {
            let target_name = match &i.ty {
                Type::Named(n) => n.name.as_str(),
                Type::Generic(g) => g.name.as_str(),
                _ => continue,
            };
            if target_name != prim_name || i.trait_type.is_some() {
                continue;
            }
            for c in &i.constants {
                if c.is_pub {
                    constants.push(build_doc_const(c, trivia));
                }
            }
        }

        if constants.is_empty() && methods.is_empty() && trait_impls.is_empty() {
            continue;
        }

        let entry = by_name
            .entry(prim_name)
            .or_insert_with(|| DocPrimitiveType {
                name: prim_name.to_string(),
                doc: None,
                constants: Vec::new(),
                methods: Vec::new(),
                trait_impls: Vec::new(),
            });
        entry.constants.extend(constants);
        entry.methods.extend(methods);
        entry.trait_impls.extend(trait_impls);
    }

    by_name.into_values().collect()
}
