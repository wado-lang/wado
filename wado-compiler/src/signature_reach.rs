//! An item reaches no further than the declarations its signature names.
//!
//! See `docs/spec.md`, "Signature reach".

use crate::ast::{
    AstId, AstVisitor, Block, Function, GenericParam, ImplBlock, Item, Module, SelfKind,
    TraitBound, Type, Visibility,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::resolve::Resolutions;
use crate::token::Span;

/// An item whose signature names a declaration that stops short of it.
#[derive(Debug)]
pub struct SignatureReachViolation {
    item: String,
    item_reach: Visibility,
    named: String,
    named_reach: Visibility,
    span: Span,
}

impl From<SignatureReachViolation> for Diagnostic {
    fn from(v: SignatureReachViolation) -> Self {
        let SignatureReachViolation {
            item,
            item_reach,
            named,
            named_reach,
            span,
        } = v;
        Diagnostic {
            severity: Severity::Error,
            code: Code::PrivateSymbol,
            message: format!(
                "`{item}` is {} but its signature names `{named}`, which is {}; \
                 widen `{named}`, or narrow `{item}`",
                reach_phrase(item_reach),
                reach_phrase(named_reach),
            ),
            span: Some(DiagnosticSpan::from_span(&span, None)),
        }
    }
}

fn reach_phrase(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Private => "private to its defining file",
        Visibility::Internal => "`internal` to its package",
        Visibility::Public => "`pub`",
    }
}

/// Every signature in `modules` that names a declaration reaching less far than
/// the item itself.
pub fn violations(
    modules: &IndexMap<ModuleSource, Module>,
    resolutions: &Resolutions,
) -> Vec<(ModuleSource, SignatureReachViolation)> {
    let mut out = Vec::new();
    for (source, module) in modules {
        let mut walk = Walk {
            resolutions,
            source,
            out: &mut out,
        };
        for item in &module.items {
            walk.item(item);
        }
    }
    out
}

/// `export` already implies `pub`, and the CM boundary asks its own question.
fn reach(visibility: Visibility, is_export: bool) -> Visibility {
    if is_export {
        Visibility::Public
    } else {
        visibility
    }
}

struct Walk<'a> {
    resolutions: &'a Resolutions,
    source: &'a ModuleSource,
    out: &'a mut Vec<(ModuleSource, SignatureReachViolation)>,
}

impl Walk<'_> {
    fn item(&mut self, item: &Item) {
        match item {
            Item::Function(f) => self.function(f, reach(f.visibility, f.is_export)),
            Item::Impl(block) => {
                let head = self.head_reach(block);
                // A trait impl's member carries no modifier of its own, so the
                // head is all that decides; an inherent one's own claim caps.
                let claimed = |declared| match block.trait_type {
                    Some(_) => head,
                    None => Visibility::narrower(declared, head),
                };
                for binding in &block.associated_types {
                    self.ty(&binding.ty, &binding.name, head);
                }
                for method in &block.methods {
                    let declared = reach(method.visibility, method.is_export);
                    self.function(method, claimed(declared));
                }
                for c in &block.constants {
                    self.ty(&c.ty, &c.name, claimed(c.visibility));
                }
            }
            Item::Trait(decl) => {
                self.bounds(&decl.supertraits, &decl.name, decl.visibility);
                self.type_params(&decl.type_params, &decl.name, decl.visibility);
                for assoc in &decl.associated_types {
                    self.bounds(&assoc.bounds, &assoc.name, decl.visibility);
                }
                for method in &decl.methods {
                    self.function(method, decl.visibility);
                }
            }
            Item::Interface(decl) => {
                for method in &decl.methods {
                    self.function(method, decl.visibility);
                }
            }
            Item::Resource(decl) => {
                self.type_params(&decl.type_params, &decl.name, decl.visibility);
                if let Some(parent) = &decl.parent {
                    self.ty(parent, &decl.name, decl.visibility);
                }
                for method in &decl.methods {
                    self.function(method, decl.visibility);
                }
            }
            Item::Struct(decl) => {
                self.type_params(&decl.type_params, &decl.name, decl.visibility);
                for field in &decl.fields {
                    let field_reach = field.visibility.narrower(decl.visibility);
                    self.ty(&field.ty, &decl.name, field_reach);
                }
            }
            Item::Variant(decl) => {
                self.type_params(&decl.type_params, &decl.name, decl.visibility);
                for payload in decl.cases.iter().filter_map(|case| case.payload.as_ref()) {
                    self.ty(payload, &decl.name, decl.visibility);
                }
            }
            Item::Newtype(decl) => {
                self.type_params(&decl.type_params, &decl.name, decl.visibility);
                self.ty(&decl.ty, &decl.name, decl.visibility);
            }
            Item::Global(decl) => self.ty(&decl.ty, &decl.name, decl.visibility),
            Item::Use(_)
            | Item::Enum(_)
            | Item::Flags(_)
            | Item::TupleTypeDecl(_)
            | Item::BuiltinTypeDecl(_)
            | Item::World(_)
            | Item::Test(_)
            | Item::Error(_) => {}
        }
    }

    fn function(&mut self, f: &Function, item_reach: Visibility) {
        self.type_params(&f.type_params, &f.name, item_reach);
        for param in &f.params {
            // A receiver names the impl target, not a type the caller writes.
            if matches!(param.self_kind, SelfKind::None) {
                self.ty(&param.ty, &f.name, item_reach);
            }
        }
        if let Some(ret) = &f.return_type {
            self.ty(ret, &f.name, item_reach);
        }
    }

    fn type_params(&mut self, params: &[GenericParam], item: &str, item_reach: Visibility) {
        self.check(item, item_reach, |sites| sites.visit_generic_params(params));
    }

    fn bounds(&mut self, bounds: &[TraitBound], item: &str, item_reach: Visibility) {
        self.check(item, item_reach, |sites| sites.visit_trait_bounds(bounds));
    }

    fn ty(&mut self, ty: &Type, item: &str, item_reach: Visibility) {
        self.check(item, item_reach, |sites| sites.visit_type(ty));
    }

    fn check(&mut self, item: &str, item_reach: Visibility, collect: impl FnOnce(&mut Sites)) {
        let mut sites = Sites(Vec::new());
        collect(&mut sites);
        for (id, span) in sites.0 {
            self.site(id, span, item, item_reach);
        }
    }

    fn site(&mut self, id: AstId, span: Span, item: &str, item_reach: Visibility) {
        let Some(def) = self.resolutions.declared(id) else {
            return;
        };
        let defs = self.resolutions.defs();
        let named_reach = defs.visibility(def);
        if named_reach < item_reach {
            self.out.push((
                self.source.clone(),
                SignatureReachViolation {
                    item: item.to_string(),
                    item_reach,
                    named: defs.name(def).to_string(),
                    named_reach,
                    span,
                },
            ));
        }
    }

    /// The reach of the declaration `id` names. A binder carries none.
    fn site_reach(&self, id: AstId) -> Option<Visibility> {
        let def = self.resolutions.declared(id)?;
        Some(self.resolutions.defs().visibility(def))
    }

    /// How far an impl's members reach. An impl declares no visibility of its
    /// own, so it takes one from what a caller must write to select it: the
    /// head, and the bounds gating it — a type that cannot name `T`'s bound
    /// cannot satisfy it, and so never reaches the members.
    fn head_reach(&self, block: &ImplBlock) -> Visibility {
        let mut sites = Sites(Vec::new());
        for ty in block.trait_type.iter().chain([&block.ty]) {
            sites.visit_type(ty);
        }
        sites.visit_generic_params(&block.type_params);
        sites
            .0
            .into_iter()
            .filter_map(|(id, _)| self.site_reach(id))
            .fold(Visibility::Public, Visibility::narrower)
    }
}

/// Collects the reference sites under whatever it is pointed at, through the
/// AST's own walkers so that a shape they reach is never silently exempt.
///
/// What it leaves out is what the name resolver does not answer for: an effect
/// name, which `effect_check` checks, and a binder's own id, which declares
/// rather than references. A body is left out because it names nothing the
/// caller has to write.
struct Sites(Vec<(AstId, Span)>);

impl AstVisitor for Sites {
    fn visit_id(&mut self, id: AstId, span: Span) {
        self.0.push((id, span));
    }

    fn visit_effect_name(&mut self, _name: &str, _id: AstId, _span: Span) {}

    fn visit_block(&mut self, _block: &Block) {}

    fn visit_generic_params(&mut self, params: &[GenericParam]) {
        for p in params {
            self.visit_trait_bounds(&p.bounds);
            if let Some(default) = &p.default {
                self.visit_type(default);
            }
        }
    }
}
