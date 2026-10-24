use crate::hashmap::{IndexMap, IndexSet};
use serde::Serialize;

use crate::ast::{
    AssociatedConst, AstId, Attribute, EnumDecl, FlagsDecl, Function, GenericParam, GlobalDecl,
    ImplBlock, InterfaceDecl, Item, Module, Newtype, Param, ResourceDecl, SelfKind, StructDecl,
    StructField, TraitBound, TraitDecl, Type, UseItem, VariantDecl, Visibility, type_head_name,
    written_params,
};
use crate::attribute::SYNOPSIS;
use crate::comment::{Comment, CommentKind, TriviaMap};
use crate::lexer::is_ident_continue;
use crate::loader::resolve_wasm_asset_path;
use crate::module_source::{ModuleSource, ModuleSourceInterner, WasmAssetKind};
use crate::primitive::PrimitiveType;
use crate::semantics::Semantics;
use crate::token::Span;
use crate::unparse::{
    get_item_id, unparse_assoc_type_decl, unparse_attributes, unparse_bound_arguments_into,
    unparse_contract_clause, unparse_enum_signature, unparse_function_signature,
    unparse_struct_signature, unparse_trait_header, unparse_type_into,
};
use crate::use_declaration;
use crate::wit_consume::build_bindings;
use crate::{ParseResult, ast, parse, stdlib};
use wit_component::DecodedWasm;

#[derive(Debug, Clone, Serialize)]
pub struct DocModule {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub synopsis: Vec<String>,
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
    /// Each associated type as declared, parameters and bounds included:
    /// `Buf<E: Elem>: Len`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub associated_types: Vec<String>,
    pub methods: Vec<DocFunction>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DocStruct {
    pub name: String,
    pub signature: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc: Option<String>,
    pub fields: Vec<DocField>,
    pub has_private_fields: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub constants: Vec<DocFunction>,
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
    /// The declaration's attributes, rendered as written. Beside the signature
    /// rather than in it: a signature is one line, and a reader wants both.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attrs: Vec<String>,
    /// The `contract` clauses, one line of source each: what a caller owes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contracts: Vec<String>,
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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub constants: Vec<DocFunction>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub methods: Vec<DocFunction>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trait_impls: Vec<DocTraitImpl>,
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

/// The program a page's module is loaded in, read at that module. The page
/// parses its own source, for the comments, and asks this what a name in it
/// resolves to. The program has no errors, so every name resolves.
pub struct DocScope<'a> {
    sem: &'a Semantics,
    module: ModuleSource,
}

impl<'a> DocScope<'a> {
    /// The program's entry module.
    #[must_use]
    pub fn of_entry(sem: &'a Semantics) -> Self {
        Self::in_module(sem, sem.entry_module_source.clone())
    }

    /// `module_spec` as the program's entry imports it.
    #[must_use]
    pub fn new(sem: &'a Semantics, module_spec: &str) -> Self {
        Self::in_module(sem, sem.resolve_notation_module(module_spec))
    }

    fn in_module(sem: &'a Semantics, module: ModuleSource) -> Self {
        assert!(sem.is_complete(), "a page reads a complete analysis");
        assert!(
            sem.modules.contains_key(&module),
            "the analysis loaded the page's module {module}"
        );
        Self { sem, module }
    }

    /// The module `module_spec` names, imported from this one.
    fn at(&self, module_spec: &str) -> Self {
        Self::in_module(
            self.sem,
            self.sem.resolve_module_from(&self.module, module_spec),
        )
    }

    /// What `b`, a block of this module in the page's own parse, implements,
    /// and the `contract` clauses that bind each of its methods.
    fn analyzed_impl(&self, b: &ImplBlock) -> (Option<&'a Item>, Vec<Vec<String>>) {
        let block = self.sem.impl_block_at(&self.module, b.span);
        let contracts = block
            .methods
            .iter()
            .map(|m| self.sem.method_contracts(m))
            .collect();
        (self.sem.implemented(block), contracts)
    }
}

/// An `impl` block the page documents, with the `contract` clauses that bind
/// each of its methods, in their order.
struct DocImpl<'a> {
    block: &'a ImplBlock,
    method_contracts: Vec<Vec<String>>,
}

/// Extract the documentation of a module. `include_private` documents non-`pub`
/// items, fields, and inherent methods too (the `wado doc --all` view); without
/// it the page is the public API.
pub fn extract_doc_with(
    module: &Module,
    trivia: &TriviaMap,
    source: &str,
    module_name: &str,
    include_private: bool,
    scope: &DocScope,
) -> DocModule {
    extract_doc_filtered(
        module,
        trivia,
        source,
        module_name,
        include_private,
        false,
        Some(scope),
    )
}

/// Like [`extract_doc_with`], but `include_internal` also admits top-level
/// `internal` items, while their members stay at the `include_private` level.
/// Lets a `pub use` re-export publish an `internal` item without leaking its
/// own private fields / methods. `scope` is `None` for a module the analysis
/// did not load: a component's bindings, which implement no trait.
fn extract_doc_filtered(
    module: &Module,
    trivia: &TriviaMap,
    source: &str,
    module_name: &str,
    include_private: bool,
    include_internal: bool,
    scope: Option<&DocScope>,
) -> DocModule {
    let module_doc = extract_module_doc(trivia, module);
    let synopsis = collect_synopsis(module, source);

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
    let impls = documented_impls(module, include_private, scope);

    for item in &module.items {
        let visible = include_private
            || is_public_or_export(item)
            || (include_internal && is_internal_item(item));
        if !visible {
            continue;
        }
        match item {
            Item::Trait(t) => traits.push(build_doc_trait(t, trivia)),
            Item::Struct(s) => structs.push(build_doc_struct(s, &impls, trivia, include_private)),
            Item::Newtype(t) => types.push(build_doc_type(t, &impls, trivia, include_private)),
            Item::Global(g) => globals.push(build_doc_global(g, trivia)),
            Item::Enum(e) => enums.push(build_doc_enum(e, trivia)),
            Item::Variant(v) => variants.push(build_doc_variant(v, trivia)),
            Item::Flags(f) => flags.push(build_doc_flags(f, trivia)),
            Item::Interface(e) => effects.push(build_doc_interface(e, trivia)),
            Item::Resource(r) => {
                resources.push(build_doc_resource(r, &impls, trivia, include_private));
            }
            Item::Function(f) => functions.push(build_doc_function(f, trivia)),
            _ => {}
        }
    }

    DocModule {
        name: module_name.to_string(),
        doc: module_doc,
        synopsis,
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

fn collect_synopsis(module: &Module, source: &str) -> Vec<String> {
    module
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Test(t) if t.attributes.iter().any(|a| a.name == SYNOPSIS) => {
                let span = t.body.span;
                let inner = source.get(span.start + 1..span.end.saturating_sub(1))?;
                Some(dedent_synopsis(inner))
            }
            _ => None,
        })
        .collect()
}

fn dedent_synopsis(body: &str) -> String {
    let lines: Vec<&str> = body
        .trim_matches(|c| c == '\n' || c == '\r')
        .lines()
        .collect();
    let indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|l| if l.len() >= indent { &l[indent..] } else { l })
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
}

fn build_doc_trait(t: &TraitDecl, trivia: &TriviaMap) -> DocTrait {
    let sig = unparse_trait_header(t);

    let associated_types: Vec<String> = t
        .associated_types
        .iter()
        .map(unparse_assoc_type_decl)
        .collect();

    let methods: Vec<DocFunction> = t
        .methods
        .iter()
        .filter(|m| !m.is_contract_holder())
        .map(|m| {
            let contracts = t.method_contracts(&m.name).map(unparse_contract_clause);
            build_doc_function_with(m, contracts.collect(), trivia)
        })
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

fn build_doc_struct(
    s: &StructDecl,
    impls: &[DocImpl],
    trivia: &TriviaMap,
    include_private: bool,
) -> DocStruct {
    // A hidden field still flags the struct as having hidden state via
    // `has_private_fields`, so the rendered signature gets a `..`
    // placeholder. With `include_private`, nothing is hidden.
    let is_hidden = |f: &StructField| !include_private && !f.visibility.is_public();
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

    let (methods, trait_impls) =
        collect_impl_methods_for_type(&s.name, impls, trivia, include_private);
    let constants = collect_impl_constants_for_type(&s.name, impls, trivia, include_private);

    DocStruct {
        name: s.name.clone(),
        signature: unparse_struct_signature(s, !include_private),
        doc: extract_doc_comment_with_attrs(trivia, s.id, &s.span, &s.attrs),
        fields,
        has_private_fields,
        constants,
        methods,
        trait_impls,
    }
}

fn build_doc_type(
    t: &Newtype,
    impls: &[DocImpl],
    trivia: &TriviaMap,
    include_private: bool,
) -> DocType {
    let mut sig = String::new();
    if t.visibility.is_public() {
        sig.push_str("pub ");
    }
    sig.push_str("type ");
    sig.push_str(&t.name);
    sig.push_str(" = ");
    sig.push_str(&render_type(&t.ty));

    let (methods, trait_impls) =
        collect_impl_methods_for_type(&t.name, impls, trivia, include_private);
    DocType {
        name: t.name.clone(),
        base_type: render_type(&t.ty),
        signature: sig,
        doc: extract_doc_comment(trivia, t.id, &t.span),
        constants: collect_impl_constants_for_type(&t.name, impls, trivia, include_private),
        methods,
        trait_impls,
    }
}

fn build_doc_global(g: &GlobalDecl, trivia: &TriviaMap) -> DocGlobal {
    let mut sig = String::new();
    if g.visibility.is_public() {
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
        signature: unparse_enum_signature(e),
        cases,
        doc: extract_doc_comment_with_attrs(trivia, e.id, &e.span, &e.attrs),
    }
}

fn build_doc_variant(v: &VariantDecl, trivia: &TriviaMap) -> DocVariant {
    let mut sig = String::new();
    if v.visibility.is_public() {
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
    if f.visibility.is_public() {
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
    if e.visibility.is_public() {
        sig.push_str("pub ");
    }
    sig.push_str("interface ");
    sig.push_str(&e.name);

    let methods: Vec<DocFunction> = e
        .methods
        .iter()
        .map(|m| build_doc_operation(m, trivia))
        .collect();

    DocEffect {
        name: e.name.clone(),
        signature: sig,
        methods,
        doc: extract_doc_comment_with_attrs(trivia, e.id, &e.span, &e.attrs),
    }
}

/// A resource's declared members plus, like a struct, whatever an `impl` adds —
/// `Stream<T>::read_to_end` is not in the `resource` body.
fn build_doc_resource(
    r: &ResourceDecl,
    impls: &[DocImpl],
    trivia: &TriviaMap,
    include_private: bool,
) -> DocResource {
    let mut sig = String::new();
    if r.visibility.is_public() {
        sig.push_str("pub ");
    }
    sig.push_str("resource ");
    sig.push_str(&r.name);
    sig.push_str(&render_generic_params(&r.type_params));

    let mut methods: Vec<DocFunction> = r
        .methods
        .iter()
        .map(|m| build_doc_operation(m, trivia))
        .collect();
    let (inherent, _) = collect_impl_methods_for_type(&r.name, impls, trivia, include_private);
    methods.extend(inherent);

    DocResource {
        name: r.name.clone(),
        signature: sig,
        methods,
        doc: extract_doc_comment_with_attrs(trivia, r.id, &r.span, &r.attrs),
    }
}

/// An interface operation or a resource method, neither of which the parser
/// lets carry a `contract` clause.
fn build_doc_operation(m: &Function, trivia: &TriviaMap) -> DocFunction {
    DocFunction {
        signature: render_interface_method_signature(m),
        attrs: unparse_attributes(&m.attrs),
        contracts: Vec::new(),
        doc: extract_doc_comment_with_attrs(trivia, m.id, &m.span, &m.attrs),
    }
}

fn build_doc_function(f: &Function, trivia: &TriviaMap) -> DocFunction {
    build_doc_function_with(f, own_contracts(f), trivia)
}

fn own_contracts(f: &Function) -> Vec<String> {
    f.contracts().map(unparse_contract_clause).collect()
}

/// `contracts` are the clauses that bind `f`, which a trait method's trait
/// declares.
fn build_doc_function_with(
    f: &Function,
    contracts: Vec<String>,
    trivia: &TriviaMap,
) -> DocFunction {
    DocFunction {
        signature: unparse_function_signature(f),
        attrs: unparse_attributes(&f.attrs),
        contracts,
        doc: extract_doc_comment_with_attrs(trivia, f.id, &f.span, &f.attrs),
    }
}

/// Extract doc text from a comment.
///
/// The lexer stores comment text *after* the `//` prefix.
/// So `/// foo` → text is `/ foo`, and `//! foo` → text is `! foo`.
/// We need to strip the leading `/` or `!` and the optional space.
fn doc_text(comment: &Comment) -> &str {
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
    let mut doc_comments: Vec<&Comment> = Vec::new();

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

fn is_public_or_export(item: &Item) -> bool {
    // `impl` carries no visibility but always contributes; `export` implies pub.
    matches!(item, Item::Impl(_))
        || matches!(item, Item::Function(f) if f.is_export)
        || item.visibility().is_some_and(Visibility::is_public)
}

fn is_internal_item(item: &Item) -> bool {
    item.visibility().is_some_and(Visibility::is_internal)
}

/// The `impl` blocks the page documents: every one under `include_private`,
/// else those whose trait or interface, if any, is `pub`. A reader who cannot
/// name a trait cannot call its methods, so listing them would advertise an
/// API that does not exist.
fn documented_impls<'a>(
    module: &'a Module,
    include_private: bool,
    scope: Option<&DocScope<'a>>,
) -> Vec<DocImpl<'a>> {
    module
        .items
        .iter()
        .filter_map(|item| {
            let Item::Impl(block) = item else {
                return None;
            };
            let (implemented, method_contracts) = if let Some(scope) = scope {
                scope.analyzed_impl(block)
            } else {
                assert!(
                    block.trait_type.is_none(),
                    "a module no analysis loaded implements no trait"
                );
                (None, block.methods.iter().map(own_contracts).collect())
            };
            (include_private || implemented.is_none_or(Item::is_public)).then_some(DocImpl {
                block,
                method_contracts,
            })
        })
        .collect()
}

fn render_type(ty: &Type) -> String {
    let mut out = String::new();
    unparse_type_into(ty, &mut out);
    out
}

fn render_generic_params(params: &[GenericParam]) -> String {
    if written_params(params).next().is_none() {
        return String::new();
    }
    let mut out = String::from("<");
    for (i, param) in written_params(params).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&param.name);
        if !param.bounds.is_empty() {
            out.push_str(": ");
            render_trait_bounds(&param.bounds, &mut out);
        }
        if let Some(default) = &param.default {
            out.push_str(" = ");
            out.push_str(&render_type(default));
        }
    }
    out.push('>');
    out
}

/// `A + B<Item = i32>` — a `+`-joined bound list, as written.
fn render_trait_bounds(bounds: &[TraitBound], out: &mut String) {
    for (i, bound) in bounds.iter().enumerate() {
        if i > 0 {
            out.push_str(" + ");
        }
        out.push_str(&bound.name);
        unparse_bound_arguments_into(bound, out);
    }
}

fn render_param(param: &Param) -> String {
    match param.self_kind {
        SelfKind::Value => "self".to_string(),
        SelfKind::Ref => "&self".to_string(),
        SelfKind::MutRef => "&mut self".to_string(),
        SelfKind::None => {
            format!("{}: {}", param.name, render_type(&param.ty))
        }
    }
}

fn render_interface_method_signature(m: &ast::Function) -> String {
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
fn parse_stdlib_for_doc(label: &str, source: &str) -> ParseResult {
    let parsed = parse(source);
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

/// The source of a program that loads the stdlib module `module_name`, for the
/// [`Semantics`] its page reads.
#[must_use]
pub fn stdlib_doc_entry(module_name: &str) -> String {
    use_declaration("_", module_name)
}

/// Extract documentation from a stdlib module by name.
///
/// For `core:prelude`, follows `pub use` re-exports and merges items from
/// the sub-modules into a single `DocModule`. For other modules, extracts
/// docs directly from the source. `include_private` is as in
/// [`extract_doc_with`].
///
/// Returns `None` if the module name is not a known stdlib module. `sem`
/// analyzes [`stdlib_doc_entry`] for `module_name`.
pub fn extract_stdlib_doc_with(
    module_name: &str,
    include_private: bool,
    sem: &Semantics,
) -> Option<DocModule> {
    let source = stdlib::get_stdlib_module(module_name)?;
    let parsed = parse_stdlib_for_doc(module_name, source);
    let scope = DocScope::new(sem, module_name);
    let mut doc = extract_doc_with(
        &parsed.ast,
        &parsed.trivia,
        source,
        module_name,
        include_private,
        &scope,
    );

    // For modules with pub use re-exports, follow them to get the actual items
    let reexport_sources = collect_pub_use_sources(&parsed.ast);
    if !reexport_sources.is_empty() {
        let exported_names = collect_pub_use_names(&parsed.ast);
        for reexport_source in &reexport_sources {
            if let Some(sub_doc) = component_reexport_doc(module_name, reexport_source) {
                merge_reexported_items(&mut doc, &sub_doc, &exported_names);
                continue;
            }
            if let Some(sub_source) = stdlib::get_stdlib_module(reexport_source) {
                let sub_parsed = parse_stdlib_for_doc(reexport_source, sub_source);
                // Admit `internal` items so a `pub use` re-export can publish
                // them, but keep `include_private = false` to hide their fields.
                let sub_doc = extract_doc_filtered(
                    &sub_parsed.ast,
                    &sub_parsed.trivia,
                    sub_source,
                    reexport_source,
                    false,
                    true,
                    Some(&scope.at(reexport_source)),
                );
                merge_reexported_items(&mut doc, &sub_doc, &exported_names);
            }
        }
    }

    // Follow `use _ from "..."` (side-effect imports) to collect impl blocks on primitive types
    let side_effect_sources = collect_side_effect_import_sources(&parsed.ast);
    for se_source in &side_effect_sources {
        if let Some(sub_source) = stdlib::get_stdlib_module(se_source) {
            let sub_parsed = parse_stdlib_for_doc(se_source, sub_source);
            let prim_types = collect_primitive_types_from_module(
                &sub_parsed.ast,
                &sub_parsed.trivia,
                include_private,
                &scope.at(se_source),
            );
            merge_primitive_types(&mut doc.primitive_types, prim_types);
        }
    }

    // Also collect from any pub use re-export sources that may have primitive impls
    for reexport_source in &reexport_sources {
        if let Some(sub_source) = stdlib::get_stdlib_module(reexport_source) {
            let sub_parsed = parse_stdlib_for_doc(reexport_source, sub_source);
            let sub_scope = scope.at(reexport_source);
            // Recursively follow side-effect imports from re-exported sub-modules
            let sub_se_sources = collect_side_effect_import_sources(&sub_parsed.ast);
            for sub_se in &sub_se_sources {
                if let Some(se_source) = stdlib::get_stdlib_module(sub_se) {
                    let se_parsed = parse_stdlib_for_doc(sub_se, se_source);
                    let prim_types = collect_primitive_types_from_module(
                        &se_parsed.ast,
                        &se_parsed.trivia,
                        include_private,
                        &sub_scope.at(sub_se),
                    );
                    merge_primitive_types(&mut doc.primitive_types, prim_types);
                }
            }
            // Also check the re-exported module itself
            let prim_types = collect_primitive_types_from_module(
                &sub_parsed.ast,
                &sub_parsed.trivia,
                include_private,
                &sub_scope,
            );
            merge_primitive_types(&mut doc.primitive_types, prim_types);
        }
    }

    Some(doc)
}

/// Collect source paths from `pub use { ... } from "source"` declarations.
/// Items a stdlib module re-exports from a bundled component asset
/// (`pub use { X } from "./icu.wat"`). The types come from the component's WIT
/// rather than from Wado source, so they carry no doc comments.
fn component_reexport_doc(module_name: &str, reexport_source: &str) -> Option<DocModule> {
    let mut interner = ModuleSourceInterner::new();
    let from = interner.core(module_name.strip_prefix("core:")?);
    let path = resolve_wasm_asset_path(&from, reexport_source, "").ok()?;
    let bytes = stdlib::get_stdlib_wasm_asset(&path)?;
    let binary = wat::parse_bytes(bytes).ok()?;
    let decoded = wit_component::decode(&binary).ok()?;
    let DecodedWasm::Component(resolve, world) = decoded else {
        return None;
    };
    let component = interner.wasm(&path, WasmAssetKind::Wat);
    let bindings = build_bindings(&resolve, world, &component).ok()?;
    Some(extract_doc_filtered(
        &bindings.module,
        &TriviaMap::default(),
        "",
        &path,
        false,
        true,
        None,
    ))
}

fn collect_pub_use_sources(module: &Module) -> Vec<String> {
    let mut sources = IndexSet::default();
    for item in &module.items {
        if let Item::Use(u) = item
            && u.visibility.is_public()
            && !u.items.iter().any(|i| matches!(i, UseItem::Wildcard))
        {
            // A module re-exported by two statements is still one module, and
            // the names are collected across all of them; walking it twice
            // would publish every item it names twice.
            sources.insert(u.source.value.clone());
        }
    }
    sources.into_iter().collect()
}

/// Collect item names from `pub use { Name1, Name2 } from "..."` declarations.
fn collect_pub_use_names(module: &Module) -> IndexSet<String> {
    let mut names = IndexSet::default();
    for item in &module.items {
        if let Item::Use(u) = item
            && u.visibility.is_public()
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

/// Rewrite a re-exported item's signature to read `pub`: the re-export
/// publishes it regardless of its own modifier.
fn promote_reexport_signature(sig: &str) -> String {
    if let Some(rest) = sig.strip_prefix("internal ") {
        format!("pub {rest}")
    } else if sig.starts_with("pub ") || sig.starts_with("export ") {
        sig.to_string()
    } else {
        format!("pub {sig}")
    }
}

/// Merge re-exported sub-module items into the parent doc, presented as `pub`
/// (see [`promote_reexport_signature`]).
fn merge_reexported_items(parent: &mut DocModule, child: &DocModule, names: &IndexSet<String>) {
    macro_rules! merge {
        ($field:ident, $it:ident => $key:expr) => {
            for $it in &child.$field {
                let key: &str = $key;
                if names.contains(key) {
                    let mut cloned = $it.clone();
                    cloned.signature = promote_reexport_signature(&cloned.signature);
                    parent.$field.push(cloned);
                }
            }
        };
    }
    merge!(traits, it => extract_item_name(&it.signature, "trait "));
    merge!(structs, it => extract_item_name(&it.signature, "struct "));
    merge!(types, it => it.name.as_str());
    merge!(globals, it => it.name.as_str());
    merge!(enums, it => extract_item_name(&it.signature, "enum "));
    merge!(variants, it => it.name.as_str());
    merge!(flags, it => it.name.as_str());
    merge!(effects, it => it.name.as_str());
    merge!(resources, it => it.name.as_str());
    merge!(functions, it => extract_item_name(&it.signature, "fn "));
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
    let end = rest
        .find(|c: char| !is_ident_continue(c))
        .unwrap_or(rest.len());
    &rest[..end]
}

fn collect_impl_methods_for_type(
    type_name: &str,
    impls: &[DocImpl],
    trivia: &TriviaMap,
    include_private: bool,
) -> (Vec<DocFunction>, Vec<DocTraitImpl>) {
    let mut inherent_methods = Vec::new();
    let mut trait_impls = Vec::new();

    for DocImpl {
        block: i,
        method_contracts,
    } in impls
    {
        if type_head_name(&i.ty) != Some(type_name) {
            continue;
        }
        let methods = i.methods.iter().zip(method_contracts);

        if let Some(ref trait_ty) = i.trait_type {
            let methods: Vec<DocFunction> = methods
                .map(|(m, contracts)| build_doc_function_with(m, contracts.clone(), trivia))
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
            for (m, contracts) in methods {
                if include_private || m.visibility.is_public() || m.is_export {
                    inherent_methods.push(build_doc_function_with(m, contracts.clone(), trivia));
                }
            }
        }
    }

    (inherent_methods, trait_impls)
}

/// The associated constants of the inherent `impl` blocks on `type_name`.
fn collect_impl_constants_for_type(
    type_name: &str,
    impls: &[DocImpl],
    trivia: &TriviaMap,
    include_private: bool,
) -> Vec<DocFunction> {
    let mut constants = Vec::new();
    for DocImpl { block: i, .. } in impls {
        if type_head_name(&i.ty) != Some(type_name) || i.trait_type.is_some() {
            continue;
        }
        for c in &i.constants {
            if include_private || c.visibility.is_public() {
                constants.push(build_doc_const(c, trivia));
            }
        }
    }
    constants
}

fn build_doc_const(c: &AssociatedConst, trivia: &TriviaMap) -> DocFunction {
    let mut sig = String::new();
    if c.visibility.is_public() {
        sig.push_str("pub ");
    }
    sig.push_str("const ");
    sig.push_str(&c.name);
    sig.push_str(": ");
    unparse_type_into(&c.ty, &mut sig);
    DocFunction {
        signature: sig,
        attrs: Vec::new(),
        contracts: Vec::new(),
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
            sources.push(u.source.value.clone());
        }
    }
    sources
}

/// Collect `impl` blocks on primitive types from a parsed module, grouping by type name.
fn collect_primitive_types_from_module(
    module: &Module,
    trivia: &TriviaMap,
    include_private: bool,
    scope: &DocScope,
) -> Vec<DocPrimitiveType> {
    let impls = documented_impls(module, include_private, Some(scope));
    let mut by_name: IndexMap<&str, DocPrimitiveType> = IndexMap::default();

    for prim_name in PrimitiveType::all_primitive_names() {
        let (methods, trait_impls) =
            collect_impl_methods_for_type(prim_name, &impls, trivia, include_private);

        let constants = collect_impl_constants_for_type(prim_name, &impls, trivia, include_private);

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
