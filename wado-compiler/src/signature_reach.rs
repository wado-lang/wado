//! An item reaches no further than the declarations its signature names.
//!
//! See `docs/spec.md`, "Signature reach".

use crate::ast::{
    AstId, Function, GenericParam, Item, Module, SelfKind, StructField, TraitBound, Type,
    Visibility,
};
use crate::compiler_host::{Code, Diagnostic, DiagnosticSpan, Severity};
use crate::hashmap::IndexMap;
use crate::module_source::ModuleSource;
use crate::resolve::{Resolution, Resolutions};
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
                // A trait impl's members reach as far as the trait, which is
                // what decides whether a caller can name them at all.
                let from_trait = block
                    .trait_type
                    .as_ref()
                    .and_then(|ty| self.declared_reach(ty));
                for method in &block.methods {
                    let member =
                        from_trait.unwrap_or_else(|| reach(method.visibility, method.is_export));
                    self.function(method, member);
                }
                for c in &block.constants {
                    let member = from_trait.unwrap_or(c.visibility);
                    self.ty(&c.ty, &c.name, member);
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
                let owner = decl.visibility;
                self.type_params(&decl.type_params, &decl.name, owner);
                for field in &decl.fields {
                    self.field(field, &decl.name, owner);
                }
            }
            Item::Variant(decl) => {
                let owner = decl.visibility;
                self.type_params(&decl.type_params, &decl.name, owner);
                for case in &decl.cases {
                    if let Some(payload) = &case.payload {
                        self.ty(payload, &decl.name, owner);
                    }
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
            // A receiver names the impl target rather than a type the caller
            // writes, so it carries no promise of its own.
            if matches!(param.self_kind, SelfKind::None) {
                self.ty(&param.ty, &f.name, item_reach);
            }
        }
        if let Some(ret) = &f.return_type {
            self.ty(ret, &f.name, item_reach);
        }
    }

    fn field(&mut self, field: &StructField, owner_name: &str, owner_reach: Visibility) {
        self.ty(
            &field.ty,
            owner_name,
            field.visibility.narrower(owner_reach),
        );
    }

    fn type_params(&mut self, params: &[GenericParam], item: &str, item_reach: Visibility) {
        for param in params {
            for bound in &param.bounds {
                self.bound(bound, item, item_reach);
            }
        }
    }

    fn bound(&mut self, bound: &TraitBound, item: &str, item_reach: Visibility) {
        self.site(bound.id, &bound.name, bound.span, item, item_reach);
        for arg in &bound.type_args {
            self.ty(arg, item, item_reach);
        }
    }

    fn ty(&mut self, ty: &Type, item: &str, item_reach: Visibility) {
        for (id, name, span) in named_sites(ty) {
            self.site(id, name, span, item, item_reach);
        }
    }

    fn site(
        &mut self,
        id: AstId,
        name: &str,
        span: Span,
        item: &str,
        item_reach: Visibility,
    ) {
        let Resolution::Def(def) = self.resolutions.get(id) else {
            return;
        };
        let named_reach = self.resolutions.defs().visibility(def);
        if named_reach < item_reach {
            self.out.push((
                self.source.clone(),
                SignatureReachViolation {
                    item: item.to_string(),
                    item_reach,
                    named: name.to_string(),
                    named_reach,
                    span,
                },
            ));
        }
    }

    /// The reach of what `ty`'s head names, when it names a declaration.
    fn declared_reach(&self, ty: &Type) -> Option<Visibility> {
        let (id, _, _) = named_sites(ty).into_iter().next()?;
        match self.resolutions.get(id) {
            Resolution::Def(def) => Some(self.resolutions.defs().visibility(def)),
            Resolution::Binder(_) | Resolution::Unresolved => None,
        }
    }
}

/// Every declaration-naming site in `ty`, head first.
fn named_sites(ty: &Type) -> Vec<(AstId, &str, Span)> {
    let mut out = Vec::new();
    collect_named_sites(ty, &mut out);
    out
}

fn collect_named_sites<'a>(ty: &'a Type, out: &mut Vec<(AstId, &'a str, Span)>) {
    match ty {
        Type::Named(t) => out.push((t.id, &t.name, t.span)),
        Type::Generic(t) => {
            out.push((t.id, &t.name, t.span));
            for a in &t.args {
                collect_named_sites(a, out);
            }
        }
        Type::NamespacedGeneric(t) => {
            for a in &t.args {
                collect_named_sites(a, out);
            }
        }
        Type::Function(ft) => {
            for p in &ft.params {
                collect_named_sites(p, out);
            }
            collect_named_sites(&ft.return_type, out);
        }
        Type::Tuple(ts) => {
            for t in ts {
                collect_named_sites(t, out);
            }
        }
        Type::Reference(t) | Type::MutReference(t) => collect_named_sites(t, out),
        Type::TypePackSpread(_, _) | Type::Infer(_) | Type::Error(_) => {}
    }
}
