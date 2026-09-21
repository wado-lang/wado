//! An item reaches no further than the declarations its signature names.
//!
//! See `docs/spec.md`, "Signature reach".

use crate::ast::{
    AstId, AstVisitor, Function, GenericParam, ImplBlock, Item, Module, SelfKind, TraitBound, Type,
    Visibility, walk_type,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::resolve::{Resolutions, head_site};
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
                let from_trait = block
                    .trait_type
                    .as_ref()
                    .and_then(|ty| self.declared_reach(ty));
                for method in &block.methods {
                    let declared =
                        from_trait.unwrap_or_else(|| reach(method.visibility, method.is_export));
                    self.function(method, declared.narrower(head));
                }
                for c in &block.constants {
                    let declared = from_trait.unwrap_or(c.visibility);
                    self.ty(&c.ty, &c.name, declared.narrower(head));
                }
            }
            Item::Trait(decl) => {
                for bound in &decl.supertraits {
                    self.bound(bound, &decl.name, decl.visibility);
                }
                self.type_params(&decl.type_params, &decl.name, decl.visibility);
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
        for bound in params.iter().flat_map(|p| &p.bounds) {
            self.bound(bound, item, item_reach);
        }
    }

    fn bound(&mut self, bound: &TraitBound, item: &str, item_reach: Visibility) {
        self.site(bound.id, bound.span, item, item_reach);
        for arg in &bound.type_args {
            self.ty(arg, item, item_reach);
        }
    }

    fn ty(&mut self, ty: &Type, item: &str, item_reach: Visibility) {
        for (id, span) in reference_sites(ty) {
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

    /// The reach of what `ty`'s head names, when it names a declaration.
    fn declared_reach(&self, ty: &Type) -> Option<Visibility> {
        self.site_reach(head_site(ty)?)
    }

    /// How far an impl's members reach: a caller has to be able to write the
    /// head to name one, so no further than what the head itself names.
    fn head_reach(&self, block: &ImplBlock) -> Visibility {
        block
            .trait_type
            .iter()
            .chain([&block.ty])
            .flat_map(reference_sites)
            .filter_map(|(id, _)| self.site_reach(id))
            .fold(Visibility::Public, Visibility::narrower)
    }
}

/// Every type reference site `ty` carries, the type's own walk deciding what
/// counts. A `with` clause is left out: it names an effect, which the name
/// resolver does not answer for and `effect_check` checks instead.
fn reference_sites(ty: &Type) -> Vec<(AstId, Span)> {
    struct Sites(Vec<(AstId, Span)>);
    impl AstVisitor for Sites {
        fn visit_id(&mut self, id: AstId, span: Span) {
            self.0.push((id, span));
        }

        fn visit_type(&mut self, ty: &Type) {
            let Type::Function(ft) = ty else {
                return walk_type(self, ty);
            };
            for param in &ft.params {
                self.visit_type(param);
            }
            self.visit_type(&ft.return_type);
        }
    }
    let mut sites = Sites(Vec::new());
    sites.visit_type(ty);
    sites.0
}
