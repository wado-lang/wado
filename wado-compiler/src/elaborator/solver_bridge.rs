//! The lowering from the compiler's tables into the solver's [`Program`], and
//! the solver's answers read back as the compiler keys them.

use crate::ast::{FunctionType, GenericParam, Type};
use crate::compiler_item::CompilerItem;
use crate::defs::{DefId, DefKind, DefTable};
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::{FqTraitName, FqTypeName, NEVER_TYPE_NAME, RefKind, TypeHead, UNIT_TYPE_NAME};
use crate::primitive::PrimitiveType;
use crate::tir::{AnonStructId, EffectRef, ResolvedType, TypeId, TypeTable};
use crate::trait_solver::{
    ArgDefault, AssocId, Candidate, Declaration, Env, Fact, ImplDef, ImplId, ImplOrigin, MethodId,
    ModuleId, ModuleScope, ParamBound, ParamDef, Pin, Program, RefRule, Selection, SolverType,
    TraitDeclId, TypeDeclId, TypeDef, applies, bound_candidates, candidates, comparison_row,
    derive, holds_with_args, mark_duplicates, owed, pair_comparisons, rank,
};

use super::trait_env::{
    BlanketReceiver, ImplHeader, ImplTargetKey, TraitDeclHeader, TraitEnv, written_arg_nodes,
};
use super::trait_query::{OnBoundTrait, primitive_has_operator};
use super::tysys::TypeSystem;
use crate::elaborator::scope;
use crate::elaborator::types::{DataDecls, TypeLookup};
use crate::resolve::{Resolution, Resolutions};
use crate::tir::StructDef;

/// What a [`TypeDeclId`] stands for: a declaration, or a shape no module
/// declares — a primitive, which the compiler answers for by name, or an
/// anonymous struct.
#[derive(PartialEq, Eq, Hash)]
enum DeclKey {
    Def(DefId),
    Builtin(String),
    /// One head for every anonymous struct, its field types as its one
    /// argument, a tuple. A literal mints its shape after the program is
    /// built, and no impl can name one, so what reaches it — its `Reflect*`
    /// facts, and the structural traits it derives over that tuple — reads the
    /// same of every shape.
    AnonymousStruct,
    /// One head for every tagged template literal's type, the fields holding
    /// its holes as the argument, on the same terms as
    /// [`Self::AnonymousStruct`].
    TemplateShape,
    /// A function type's head; `fn mut` is a shape of its own, since a closure
    /// that may write its captures is not the other.
    FnShape {
        is_mut: bool,
    },
    /// One head for every effect a function type names that the program does
    /// not declare: an effect binder, or a name that reached nothing.
    UndeclaredEffect,
}

/// Why the lowering states nothing about a type, most telling first: a type
/// holding several reasons answers to the first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Unsaid {
    /// It names a type that failed to resolve or to infer, which was reported
    /// where it failed.
    Failed,
    /// It names an inference variable, or a binder the scope does not declare.
    Open,
    /// A shape the solver has no way to say.
    Unsayable,
}

/// Every part lowered, or the most telling reason one was not.
fn said(
    parts: impl IntoIterator<Item = Result<SolverType, Unsaid>>,
) -> Result<Vec<SolverType>, Unsaid> {
    let mut out = Vec::new();
    let mut unsaid: Option<Unsaid> = None;
    for part in parts {
        match part {
            Ok(ty) => out.push(ty),
            Err(Unsaid::Failed) => return Err(Unsaid::Failed),
            Err(reason) => unsaid = Some(unsaid.map_or(reason, |u| u.min(reason))),
        }
    }
    unsaid.map_or(Ok(out), Err)
}

/// What the names in a written type mean where it is written.
struct Written<'a> {
    resolutions: &'a Resolutions,
    /// The binders in scope.
    param: &'a dyn Fn(&str) -> Option<ParamKind>,
    /// The trait among a binder's bounds declaring an associated type, which
    /// is what `T::Item` projects through.
    declaring: &'a dyn Fn(&str, &str) -> Option<DefId>,
    /// What `Self` means, where anything does.
    self_type: Option<&'a SolverType>,
}

/// The trait among the bounds `params` puts on `base` that declares `assoc`.
fn declaring_among<'p>(
    tysys: &TypeSystem,
    params: impl IntoIterator<Item = &'p GenericParam>,
    base: &str,
    assoc: &str,
) -> Option<DefId> {
    let param = params.into_iter().find(|p| p.name == base)?;
    tysys
        .trait_env
        .bound_declaring_assoc_type(&param.bounds, assoc, &tysys.resolutions)
}

/// How a declaration's parameter is spelled where a type mentions it.
#[derive(Clone)]
enum ParamKind {
    Type(u32),
    Pack(u32),
    /// `<F: fn(...)>`: the parameter is that signature, a type and no slot.
    Signature(Box<FunctionType>),
}

impl ParamKind {
    /// What `name` is among `params`.
    fn of<'p>(params: impl IntoIterator<Item = &'p GenericParam>, name: &str) -> Option<Self> {
        let (at, declared) = params
            .into_iter()
            .enumerate()
            .find(|(_, p)| p.name == name)?;
        let index = u32::try_from(at).expect("fewer than 2^32 params");
        let signature = (!declared.is_pack)
            .then(|| declared.bounds.iter().find_map(|b| b.fn_signature.as_ref()))
            .flatten();
        Some(match signature {
            Some(sig) => Self::Signature(sig.clone()),
            None if declared.is_pack => Self::Pack(index),
            None => Self::Type(index),
        })
    }

    /// The slot this parameter holds. A signature holds none, and is lowered
    /// where its written form is at hand.
    fn spelled(&self) -> Result<SolverType, Unsaid> {
        match self {
            Self::Type(index) => Ok(SolverType::Param(*index)),
            Self::Pack(index) => Ok(SolverType::Pack(*index)),
            Self::Signature(_) => Err(Unsaid::Unsayable),
        }
    }
}

/// A rigid binder a resolved type names, for the caller to place.
#[derive(Clone, Copy)]
enum Binder<'a> {
    /// A type parameter or pack, by its spelling and declared position.
    Param { name: &'a str, index: u32 },
    /// A generic associated type's own parameter, by its identity: an impl
    /// may write it under a name of its own.
    Family(TypeId),
}

/// The interning both directions share, so an impl lowered from its header
/// and a receiver lowered from the type table name one declaration by one id.
pub(super) struct Lowering {
    decls: IndexMap<DeclKey, u32>,
    /// Each declared effect's head, by the reference a function type carries
    /// it by.
    effects: IndexMap<EffectRef, TypeDeclId>,
    modules: IndexMap<ModuleSource, u32>,
    /// The declaration a tuple type is an instance of. An impl writes a tuple
    /// as `[..T]`, so an instance lowers to [`SolverType::Tuple`] as well.
    tuple: Option<DefId>,
    /// A trait's associated types, by the trait and the name.
    assocs: IndexMap<(TraitDeclId, String), u32>,
    /// Method names, interned across traits: two traits declaring `describe`
    /// share one id, which is what makes their collision one question.
    methods: IndexMap<String, u32>,
    /// The impl block each lowered impl came from, so a selection the compiler
    /// made and one the solver made name the same thing. A derived impl names
    /// the blanket its body comes from; a primitive's impl, or a body the
    /// compiler supplies with no blanket, is written by no block and is absent.
    impl_defs: IndexMap<ImplId, DefId>,
    /// The impl each written block lowered to: [`Self::impl_defs`] the other
    /// way, for a written block alone.
    written_impls: IndexMap<DefId, ImplId>,
    /// The `Reflect*`-bounded blanket a derived body comes from, by the trait
    /// and the reflection kind it bounds on. Lookup collects that block for a
    /// derived body, so a `Derived` impl is named to it.
    derivation_source: IndexMap<(TraitDeclId, CompilerItem), DefId>,
    /// Heads the program names before it knows their members: a struct or
    /// newtype declared in a body, until annotate resolves its block
    /// ([`SolverBridge::state_local`]). Only the compiler answers for one.
    unstated: IndexSet<TypeDeclId>,
}

/// The id `key` has in `map`, minted at the next index when it has none.
fn intern<K: std::hash::Hash + Eq>(map: &mut IndexMap<K, u32>, key: K) -> u32 {
    let next = u32::try_from(map.len()).expect("a program declares fewer than 2^32 items");
    *map.entry(key).or_insert(next)
}

impl Lowering {
    /// A lowering with the heads every function type lowers under interned:
    /// an impl header and a resolved type alike name them, and coherence
    /// lowers headers alone.
    pub(super) fn new() -> Self {
        let mut lowering = Self {
            decls: IndexMap::default(),
            effects: IndexMap::default(),
            modules: IndexMap::default(),
            tuple: None,
            assocs: IndexMap::default(),
            methods: IndexMap::default(),
            impl_defs: IndexMap::default(),
            written_impls: IndexMap::default(),
            derivation_source: IndexMap::default(),
            unstated: IndexSet::default(),
        };
        for key in [
            DeclKey::FnShape { is_mut: false },
            DeclKey::FnShape { is_mut: true },
            DeclKey::UndeclaredEffect,
        ] {
            intern(&mut lowering.decls, key);
        }
        lowering
    }

    /// A lowering with every head a written type can name interned: each
    /// type, trait and effect declaration, and each builtin shape.
    pub(super) fn over(resolutions: &Resolutions) -> Self {
        let mut lowering = Self::new();
        let defs = resolutions.defs();
        for def in defs.iter() {
            match defs.kind(def) {
                DefKind::Struct
                | DefKind::Enum
                | DefKind::Flags
                | DefKind::Variant
                | DefKind::Newtype
                | DefKind::BuiltinType => {
                    lowering.head_of(defs, def);
                }
                DefKind::Trait => {
                    lowering.trait_decl(def);
                }
                // A resource is also an effect to a function type's `with`
                // clause, under its own head.
                DefKind::Resource => {
                    let head = lowering.head_of(defs, def);
                    lowering.effect_named(resolutions, def, head);
                }
                // An interface is a trait to a bound, and an effect to a
                // function type's `with` clause.
                DefKind::Effect => {
                    let head = lowering.type_decl(def);
                    lowering.effect_named(resolutions, def, head);
                }
                DefKind::Function
                | DefKind::World
                | DefKind::Global
                | DefKind::Variable
                | DefKind::Field
                | DefKind::EnumCase
                | DefKind::VariantCase
                | DefKind::FlagsMember
                | DefKind::Impl
                | DefKind::Method => {}
            }
        }
        // `type_id` spells a resolved type under these heads whether or not a
        // written type names one.
        for name in [TypeTable::ARRAY_TYPE_NAME, UNIT_TYPE_NAME, NEVER_TYPE_NAME]
            .into_iter()
            .chain(PrimitiveType::all_primitive_names())
        {
            lowering.builtin(name);
        }
        lowering
    }

    /// `head` as the effect `def` declares.
    fn effect_named(&mut self, resolutions: &Resolutions, def: DefId, head: TypeDeclId) {
        let effect = resolutions
            .effect_decl(def)
            .expect("an interface or a resource is an effect");
        self.effects.insert(effect, head);
    }

    /// Every trait's associated types, which a projection reads by the
    /// declaring trait.
    pub(super) fn intern_assocs(&mut self, trait_headers: &IndexMap<DefId, TraitDeclHeader>) {
        for (&trait_, header) in trait_headers {
            let id = self.trait_decl(trait_);
            for assoc in &header.assoc_types {
                self.assoc(id, &assoc.name);
            }
        }
    }

    fn type_decl(&mut self, def: DefId) -> TypeDeclId {
        TypeDeclId(intern(&mut self.decls, DeclKey::Def(def)))
    }

    fn builtin(&mut self, name: &str) -> TypeDeclId {
        TypeDeclId(intern(&mut self.decls, DeclKey::Builtin(name.to_string())))
    }

    /// The head a written type reaching `def` lowers under, keyed as the impl
    /// index keys it.
    fn head_of(&mut self, defs: &DefTable, def: DefId) -> TypeDeclId {
        match ImplTargetKey::of_decl(defs, def) {
            ImplTargetKey::Builtin(name) => self.builtin(&name),
            ImplTargetKey::Decl(def) => self.type_decl(def),
            key => unreachable!("a declaration keys as itself or a builtin, not {key:?}"),
        }
    }

    fn anonymous_struct(&mut self) -> TypeDeclId {
        TypeDeclId(intern(&mut self.decls, DeclKey::AnonymousStruct))
    }

    /// The head every anonymous struct lowers under, interned by `build`.
    fn anonymous_head(&self) -> TypeDeclId {
        self.known_type(&DeclKey::AnonymousStruct)
            .expect("the anonymous head is interned before the program is read")
    }

    fn template_shape(&mut self) -> TypeDeclId {
        TypeDeclId(intern(&mut self.decls, DeclKey::TemplateShape))
    }

    /// The head every template shape lowers under, interned by `build`.
    fn template_head(&self) -> TypeDeclId {
        self.known_type(&DeclKey::TemplateShape)
            .expect("the template head is interned before the program is read")
    }

    /// A function type as the solver reads it: the shape, whose arguments are
    /// its parameters, its return, then its effects as one tuple, so two
    /// signatures differing only in a `with` clause are two types, and no two
    /// arities collide.
    fn fn_type(
        &self,
        is_mut: bool,
        mut signature: Vec<SolverType>,
        effects: impl IntoIterator<Item = TypeDeclId>,
    ) -> SolverType {
        let head = self
            .known_type(&DeclKey::FnShape { is_mut })
            .expect("`Lowering::new` interns both function heads");
        let effects = effects
            .into_iter()
            .map(|e| SolverType::Decl(e, Vec::new()))
            .collect();
        signature.push(SolverType::Tuple(effects));
        SolverType::Decl(head, signature)
    }

    fn effect_head(&self, effect: &EffectRef) -> TypeDeclId {
        self.effects
            .get(effect)
            .copied()
            .unwrap_or_else(|| self.undeclared_effect())
    }

    fn undeclared_effect(&self) -> TypeDeclId {
        self.known_type(&DeclKey::UndeclaredEffect)
            .expect("`Lowering::new` interns the undeclared effect head")
    }

    fn trait_decl(&mut self, def: DefId) -> TraitDeclId {
        TraitDeclId(intern(&mut self.decls, DeclKey::Def(def)))
    }

    fn assoc(&mut self, trait_: TraitDeclId, name: &str) -> AssocId {
        AssocId(intern(&mut self.assocs, (trait_, name.to_string())))
    }

    fn method(&mut self, name: &str) -> MethodId {
        MethodId(intern(&mut self.methods, name.to_string()))
    }

    fn module(&mut self, module: &ModuleSource) -> ModuleId {
        ModuleId(intern(&mut self.modules, module.clone()))
    }

    /// The id a declaration was given, if it was ever lowered.
    fn known_trait(&self, def: DefId) -> Option<TraitDeclId> {
        self.decls.get(&DeclKey::Def(def)).map(|&i| TraitDeclId(i))
    }

    fn known_type(&self, key: &DeclKey) -> Option<TypeDeclId> {
        self.decls.get(key).map(|&i| TypeDeclId(i))
    }

    /// A written trait argument as the solver spells it, `param` placing the
    /// binders of the space it is written in.
    fn named_arg(
        &self,
        name: &FqTypeName,
        param: &dyn Fn(&str) -> Option<ParamKind>,
    ) -> Result<SolverType, Unsaid> {
        let args = said(name.args().iter().map(|arg| self.named_arg(arg, param)))?;
        let pointee = match name.head() {
            TypeHead::Binder { name, .. } => param(name).ok_or(Unsaid::Open)?.spelled()?,
            TypeHead::Tuple => SolverType::Tuple(args),
            TypeHead::Builtin(builtin) => SolverType::Decl(
                self.known_type(&DeclKey::Builtin(builtin.clone()))
                    .ok_or(Unsaid::Unsayable)?,
                args,
            ),
            TypeHead::Unresolved(_) => return Err(Unsaid::Failed),
            TypeHead::Function {
                is_mut,
                signature,
                effects,
            } => self.fn_type(
                *is_mut,
                said(signature.iter().map(|ty| self.named_arg(ty, param)))?,
                effects.iter().map(|e| self.effect_head(e)),
            ),
            TypeHead::Projection(_) => {
                let projected = name.projected().expect("a projection head projects");
                let trait_ = self
                    .known_trait(projected.owning_trait)
                    .ok_or(Unsaid::Unsayable)?;
                SolverType::Projection {
                    base: Box::new(self.named_arg(projected.base, param)?),
                    trait_,
                    assoc: self
                        .known_assoc(trait_, projected.assoc)
                        .ok_or(Unsaid::Unsayable)?,
                    args,
                }
            }
            head => SolverType::Decl(
                head.def()
                    .and_then(|def| self.known_type(&DeclKey::Def(def)))
                    .ok_or(Unsaid::Unsayable)?,
                args,
            ),
        };
        Ok(name
            .references()
            .iter()
            .rev()
            .fold(pointee, |inner, kind| SolverType::Ref {
                is_mut: *kind == RefKind::Mut,
                inner: Box::new(inner),
            }))
    }

    /// A trait reference as the solver spells a bound, `param` placing the
    /// binders its arguments are written in.
    fn bound_named(
        &self,
        named: &FqTraitName,
        param: &dyn Fn(&str) -> Option<ParamKind>,
    ) -> Result<ParamBound, Unsaid> {
        let trait_ = named
            .canonical()
            .and_then(|def| self.known_trait(def))
            .ok_or(Unsaid::Unsayable)?;
        let args = said(named.args().iter().map(|arg| self.named_arg(arg, param)))?;
        Ok(ParamBound { trait_, args })
    }

    /// The declaration a trait id was given for. Every trait id is minted from
    /// one, so a builtin key here is a lowering bug.
    fn trait_def_of(&self, id: TraitDeclId) -> DefId {
        let Some((DeclKey::Def(def), _)) = self.decls.get_index(id.0 as usize) else {
            panic!("trait {id:?} was not minted from a declaration");
        };
        *def
    }

    fn known_assoc(&self, trait_: TraitDeclId, name: &str) -> Option<AssocId> {
        self.assocs
            .get(&(trait_, name.to_string()))
            .map(|&i| AssocId(i))
    }

    fn known_module(&self, module: &ModuleSource) -> Option<ModuleId> {
        self.modules.get(module).map(|&i| ModuleId(i))
    }

    /// The id of a declaration `build` interned up front.
    fn declared_type(&self, def: DefId) -> TypeDeclId {
        self.known_type(&DeclKey::Def(def))
            .expect("every declaration is interned before the program is read")
    }

    fn declared_module(&self, module: &ModuleSource) -> ModuleId {
        self.known_module(module)
            .expect("every module is interned before the program is read")
    }

    /// The head a written type reaching `def` lowers under, interned up front.
    fn known_head(&self, defs: &DefTable, def: DefId) -> Result<TypeDeclId, Unsaid> {
        let key = match ImplTargetKey::of_decl(defs, def) {
            ImplTargetKey::Builtin(name) => DeclKey::Builtin(name),
            ImplTargetKey::Decl(def) => DeclKey::Def(def),
            key => unreachable!("a declaration keys as itself or a builtin, not {key:?}"),
        };
        self.known_type(&key).ok_or(Unsaid::Unsayable)
    }

    /// One AST type as the solver reads it, in the space `written` describes.
    fn ast_type(&self, ty: &Type, written: &Written) -> Result<SolverType, Unsaid> {
        let resolutions = written.resolutions;
        let all = |types: &[Type]| said(types.iter().map(|ty| self.ast_type(ty, written)));
        let declared = |id| match resolutions.get(id) {
            Resolution::Def(def) => self.known_head(resolutions.defs(), def),
            // A name reaching nothing was reported where it is written.
            Resolution::Unresolved => Err(Unsaid::Failed),
            Resolution::Binder(_) | Resolution::Projection(_) => Err(Unsaid::Unsayable),
        };
        match ty {
            Type::Named(named) if named.name == "Self" => {
                written.self_type.cloned().ok_or(Unsaid::Unsayable)
            }
            Type::Named(named) => match (written.param)(&named.name) {
                Some(ParamKind::Signature(sig)) => self.ast_type(&Type::Function(sig), written),
                Some(kind) => kind.spelled(),
                None => Ok(SolverType::Decl(declared(named.id)?, Vec::new())),
            },
            Type::Generic(generic) => {
                let args = all(&generic.args)?;
                Ok(SolverType::Decl(declared(generic.id)?, args))
            }
            Type::Tuple(elems) => all(elems).map(SolverType::Tuple),
            Type::TypePackSpread(name, _) => match (written.param)(name).ok_or(Unsaid::Open)? {
                ParamKind::Pack(index) | ParamKind::Type(index) => Ok(SolverType::Pack(index)),
                ParamKind::Signature(_) => Err(Unsaid::Unsayable),
            },
            Type::Reference(inner) | Type::MutReference(inner) => Ok(SolverType::Ref {
                is_mut: matches!(ty, Type::MutReference(_)),
                inner: Box::new(self.ast_type(inner, written)?),
            }),
            // `T::Item` reads the trait among `T`'s bounds that declares
            // `Item`, as the elaborator resolves it.
            Type::NamespacedGeneric(projection)
                if matches!(resolutions.get(projection.id), Resolution::Projection(_)) =>
            {
                let base = if projection.namespace == "Self" {
                    written.self_type.cloned().ok_or(Unsaid::Unsayable)?
                } else {
                    (written.param)(&projection.namespace)
                        .ok_or(Unsaid::Open)?
                        .spelled()?
                };
                let trait_ = (written.declaring)(&projection.namespace, &projection.name)
                    .and_then(|def| self.known_trait(def))
                    .ok_or(Unsaid::Unsayable)?;
                Ok(SolverType::Projection {
                    base: Box::new(base),
                    trait_,
                    assoc: self
                        .known_assoc(trait_, &projection.name)
                        .ok_or(Unsaid::Unsayable)?,
                    args: all(&projection.args)?,
                })
            }
            Type::NamespacedGeneric(generic) => {
                let args = all(&generic.args)?;
                Ok(SolverType::Decl(declared(generic.id)?, args))
            }
            Type::Function(f) => {
                let signature = said(
                    f.params
                        .iter()
                        .chain(std::iter::once(&f.return_type))
                        .map(|ty| self.ast_type(ty, written)),
                )?;
                let effects: Vec<TypeDeclId> = f
                    .effects
                    .iter()
                    .map(|effect| match resolutions.get(effect.id) {
                        Resolution::Def(def) if resolutions.defs().kind(def).is_effect() => self
                            .known_type(&DeclKey::Def(def))
                            .expect("every effect is interned up front"),
                        _ => self.undeclared_effect(),
                    })
                    .collect();
                Ok(self.fn_type(f.is_mut, signature, effects))
            }
            // A placeholder no inference fills, or the parser's recovery from
            // a reported error.
            Type::Infer(_) => Err(Unsaid::Unsayable),
            Type::Error(_) => Err(Unsaid::Failed),
        }
    }

    /// One resolved type as the solver reads it. `param` gives a rigid binder
    /// its position.
    fn type_id(
        &self,
        table: &TypeTable,
        id: TypeId,
        param: &dyn Fn(Binder) -> Option<u32>,
    ) -> Result<SolverType, Unsaid> {
        let decl = |key: DeclKey, args: Vec<SolverType>| {
            self.known_type(&key)
                .map(|id| SolverType::Decl(id, args))
                .ok_or(Unsaid::Unsayable)
        };
        let placed =
            |name: &str, index: u32| param(Binder::Param { name, index }).ok_or(Unsaid::Open);
        // A pack spliced into a tuple is the pack; a mapped one (`R[F := F_i]`
        // per element) is a shape the solver has no way to say.
        let tuple_elem = |a: TypeId| {
            let ResolvedType::TypePack {
                name,
                index,
                mapped_elem,
            } = table.get(a)
            else {
                return self.type_id(table, a, param);
            };
            let pack = placed(name, *index)?;
            match mapped_elem {
                None => Ok(SolverType::Pack(pack)),
                Some(_) => Err(Unsaid::Unsayable),
            }
        };
        let instance = |def: DefId, type_args: &[TypeId]| {
            if self.tuple == Some(def) {
                return said(type_args.iter().map(|&a| tuple_elem(a))).map(SolverType::Tuple);
            }
            decl(
                DeclKey::Def(def),
                said(type_args.iter().map(|&a| self.type_id(table, a, param)))?,
            )
        };
        match table.get(id) {
            ResolvedType::Primitive(p) => decl(DeclKey::Builtin(p.as_str().to_string()), vec![]),
            ResolvedType::BuiltinArray(elem) => decl(
                DeclKey::Builtin(TypeTable::ARRAY_TYPE_NAME.to_string()),
                vec![self.type_id(table, *elem, param)?],
            ),
            // `()` is the unit declaration, not the empty tuple
            // (WEP 2026-09-01, "The candidates").
            ResolvedType::Unit => decl(DeclKey::Builtin(UNIT_TYPE_NAME.into()), vec![]),
            ResolvedType::Struct {
                def: StructDef::Decl(def),
                type_args,
            } => instance(*def, type_args),
            // A literal's shape lowers under the one anonymous head, its field
            // types as the argument. A synthetic shape — a closure
            // environment — declares no fields the compiler reflects, so it
            // stays unsaid.
            ResolvedType::Struct {
                def: StructDef::Anon(shape),
                ..
            } => {
                if let Some(template) = table.template_shape(*shape) {
                    let fields = said(template.holes.iter().map(|hole| {
                        let held = self.type_id(table, hole.ty, param)?;
                        Ok(if table.hole_held_by_ref(hole.ty) {
                            SolverType::Ref {
                                is_mut: false,
                                inner: Box::new(held),
                            }
                        } else {
                            held
                        })
                    }))?;
                    return decl(DeclKey::TemplateShape, vec![SolverType::Tuple(fields)]);
                }
                if table.anon_struct_is_synthetic(*shape) {
                    return Err(Unsaid::Unsayable);
                }
                let fields = said(
                    table
                        .anon_struct_fields(*shape)
                        .iter()
                        .map(|(_, ty)| self.type_id(table, *ty, param)),
                )?;
                decl(DeclKey::AnonymousStruct, vec![SolverType::Tuple(fields)])
            }
            ResolvedType::Enum { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Flags { def } => instance(*def, &[]),
            ResolvedType::GenericResource { def, type_args }
            | ResolvedType::GenericInstance { def, type_args }
            | ResolvedType::Newtype { def, type_args, .. } => instance(*def, type_args),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => Ok(SolverType::Ref {
                is_mut: matches!(table.get(id), ResolvedType::MutRef(_)),
                inner: Box::new(self.type_id(table, *inner, param)?),
            }),
            ResolvedType::TypeParam { name, index } => placed(name, *index).map(SolverType::Param),
            // Outside a tuple, a pack stands for one of its elements: a rigid
            // type carrying the pack's bounds, at the pack's slot. A mapped
            // one is `R` at that element.
            ResolvedType::TypePack {
                name,
                index,
                mapped_elem: None,
            } => placed(name, *index).map(SolverType::Param),
            ResolvedType::TypePack {
                mapped_elem: Some(mapped),
                ..
            } => self.type_id(table, *mapped, param),
            ResolvedType::Function {
                is_mut,
                params,
                return_type,
                effects,
            } => {
                let signature = said(
                    params
                        .iter()
                        .chain(std::iter::once(return_type))
                        .map(|&a| self.type_id(table, a, param)),
                )?;
                Ok(self.fn_type(
                    *is_mut,
                    signature,
                    effects.iter().map(|e| self.effect_head(e)),
                ))
            }
            // `impl Inspect for !` is written in the prelude, so the receiver
            // side names the same shape.
            ResolvedType::Never => decl(DeclKey::Builtin(NEVER_TYPE_NAME.into()), vec![]),
            // A projection on a rigid parameter, satisfying what its trait
            // declares of the associated type.
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                args,
                owning_trait,
                ..
            } => {
                let base = self.type_id(table, *param_id, param)?;
                let args = said(args.iter().map(|&a| self.type_id(table, a, param)))?;
                let trait_ = self.known_trait(*owning_trait).ok_or(Unsaid::Unsayable)?;
                Ok(SolverType::Projection {
                    base: Box::new(base),
                    trait_,
                    assoc: self
                        .known_assoc(trait_, assoc_name)
                        .ok_or(Unsaid::Unsayable)?,
                    args,
                })
            }
            ResolvedType::AssocParam { .. } => param(Binder::Family(id))
                .map(SolverType::Param)
                .ok_or(Unsaid::Open),
            ResolvedType::InferVar(_) => Err(Unsaid::Open),
            ResolvedType::Unknown | ResolvedType::Error => Err(Unsaid::Failed),
        }
    }
}

/// Lower the impl headers into the program, and hand back the header each
/// [`ImplId`](crate::trait_solver::ImplId) stands for. A header the lowering
/// cannot express is dropped, never approximated. A projection in a header
/// reads the trait `env` says declares it.
pub(super) fn lower_impls<'a>(
    lowering: &mut Lowering,
    program: &mut Program,
    impl_headers: impl IntoIterator<Item = (&'a DefId, &'a ImplHeader)>,
    env: &TraitEnv,
    resolutions: &Resolutions,
) -> Vec<&'a ImplHeader> {
    let mut sources: Vec<&ImplHeader> = Vec::new();
    for (&def, header) in impl_headers {
        let param = |name: &str| ParamKind::of(&header.type_params, name);
        let declaring_here = |base: &str, assoc: &str| {
            if base == "Self" {
                let implemented: Vec<DefId> = header.trait_def().into_iter().collect();
                return env
                    .trait_among_declaring_assoc_type(&implemented, assoc)
                    .or_else(|| env.impl_trait_binding_assoc(&header.target, assoc));
            }
            let traits: Vec<DefId> = header
                .type_params
                .iter()
                .filter(|p| p.name == base)
                .flat_map(|p| p.bounds.iter().filter_map(|b| resolutions.bound_decl(b)))
                .collect();
            env.trait_among_declaring_assoc_type(&traits, assoc)
        };
        let mut space = Written {
            resolutions,
            param: &param,
            declaring: &declaring_here,
            self_type: None,
        };
        let Ok(target) = lowering.ast_type(&header.ty, &space) else {
            continue;
        };
        space.self_type = Some(&target);
        let written = header.trait_ty().map_or(&[][..], written_arg_nodes);
        let Ok(trait_args) = said(written.iter().map(|arg| lowering.ast_type(arg, &space))) else {
            continue;
        };
        let implemented = header.trait_def().map(|t| lowering.trait_decl(t));
        let Some(params) = header
            .type_params
            .iter()
            .map(|p| {
                let mut def = ParamDef::default();
                for b in &p.bounds {
                    let Some(bound) = resolutions.bound_decl(b) else {
                        continue;
                    };
                    let bound = lowering.trait_decl(bound);
                    // A bound the lowering cannot spell drops the header: any
                    // other reading says more or less than it writes.
                    let args =
                        said(b.type_args.iter().map(|arg| lowering.ast_type(arg, &space))).ok()?;
                    def.bounds.push(ParamBound {
                        trait_: bound,
                        args,
                    });
                    // A pin the lowering cannot spell is dropped, as the
                    // compiler's own check drops one to anything but the
                    // receiver.
                    for constraint in &b.assoc_types {
                        let Ok(ty) = lowering.ast_type(&constraint.ty, &space) else {
                            continue;
                        };
                        def.pins.push(Pin {
                            trait_: bound,
                            assoc: lowering.assoc(bound, &constraint.name),
                            ty,
                        });
                    }
                }
                Some(def)
            })
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let id = program.push_impl(ImplDef {
            trait_: implemented,
            trait_args,
            target: target.clone(),
            params,
            origin: if header.is_synthesize_request {
                ImplOrigin::Marker
            } else {
                ImplOrigin::Written
            },
        });
        lowering.impl_defs.insert(id, def);
        lowering.written_impls.insert(def, id);
        if let Some(implemented) = implemented {
            let own = header
                .methods
                .iter()
                .map(|m| lowering.method(&m.name))
                .collect();
            program.impl_methods.insert(id, own);
            let past = u32::try_from(header.type_params.len()).expect("fewer than 2^32 params");
            for binding in &header.associated_types {
                // A family's own parameters follow the impl's, as
                // `Program::assoc_bindings` spells them.
                let in_family = |name: &str| match ParamKind::of(&binding.type_params, name) {
                    Some(ParamKind::Type(j)) => Some(ParamKind::Type(past + j)),
                    Some(ParamKind::Pack(j)) => Some(ParamKind::Pack(past + j)),
                    Some(signature @ ParamKind::Signature(_)) => Some(signature),
                    None => param(name),
                };
                let family = Written {
                    param: &in_family,
                    ..space
                };
                let Ok(ty) = lowering.ast_type(&binding.ty, &family) else {
                    continue;
                };
                program
                    .assoc_bindings
                    .entry(id)
                    .or_default()
                    .push((lowering.assoc(implemented, &binding.name), ty));
            }
        }
        sources.push(header);
    }
    sources
}

/// A type standing for a lowered head, for what the compiler reads off a type
/// rather than off an impl. `None` for a head that is no type (a trait), or
/// whose type is minted after the program is built (a body-local struct).
fn representative(
    tysys: &TypeSystem,
    table: &TypeTable,
    tuple: Option<DefId>,
    key: &DeclKey,
) -> Option<ResolvedType> {
    match key {
        // The tuple declaration registers no type of its own; an instance of
        // it is what a tuple type is.
        DeclKey::Def(def) if Some(*def) == tuple => Some(ResolvedType::GenericInstance {
            def: *def,
            type_args: Vec::new(),
        }),
        DeclKey::Def(def) => {
            let defs = tysys.resolutions.defs();
            match defs.kind(*def) {
                DefKind::Struct
                | DefKind::Enum
                | DefKind::Flags
                | DefKind::Variant
                | DefKind::Newtype
                | DefKind::BuiltinType
                | DefKind::Resource => table
                    .type_of_symbol(&defs.ast_id(*def))
                    .map(|id| table.get(id).clone()),
                DefKind::Function
                | DefKind::Effect
                | DefKind::Trait
                | DefKind::Impl
                | DefKind::Method
                | DefKind::World
                | DefKind::Global
                | DefKind::Variable
                | DefKind::Field
                | DefKind::EnumCase
                | DefKind::VariantCase
                | DefKind::FlagsMember => None,
            }
        }
        DeclKey::Builtin(name) => {
            if let Some(id) = TypeTable::primitive_by_name(name) {
                return Some(table.get(id).clone());
            }
            (name == TypeTable::ARRAY_TYPE_NAME)
                .then_some(ResolvedType::BuiltinArray(TypeTable::UNIT))
        }
        DeclKey::FnShape { is_mut } => Some(ResolvedType::Function {
            is_mut: *is_mut,
            params: Vec::new(),
            return_type: TypeTable::UNIT,
            effects: Vec::new(),
        }),
        DeclKey::AnonymousStruct | DeclKey::TemplateShape | DeclKey::UndeclaredEffect => None,
    }
}

/// The newtype declarations with their base types. A `flags` type sits in the
/// same table and is not one.
fn newtype_decls<'a>(
    data: &'a DataDecls,
    table: &'a TypeTable,
) -> impl Iterator<Item = (DefId, TypeId)> + 'a {
    data.newtypes
        .iter()
        .filter_map(|(&def, &id)| match table.get(id) {
            ResolvedType::Newtype { base_type, .. } => Some((def, *base_type)),
            _ => None,
        })
}

/// The solver's view of the whole program.
pub(crate) struct SolverBridge {
    program: Program,
    lowering: Lowering,
}

impl std::fmt::Debug for SolverBridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SolverBridge").finish_non_exhaustive()
    }
}

impl SolverBridge {
    /// The four structural traits [`derive`] generates impls for.
    const DERIVED: [CompilerItem; 4] = [
        CompilerItem::Eq,
        CompilerItem::Ord,
        CompilerItem::Serialize,
        CompilerItem::Deserialize,
    ];

    /// The operator items a primitive carries, by [`primitive_has_operator`].
    const OPERATORS: [CompilerItem; 12] = [
        CompilerItem::Add,
        CompilerItem::Sub,
        CompilerItem::Mul,
        CompilerItem::Div,
        CompilerItem::Rem,
        CompilerItem::Neg,
        CompilerItem::BitAnd,
        CompilerItem::BitOr,
        CompilerItem::BitXor,
        CompilerItem::BitNot,
        CompilerItem::Shl,
        CompilerItem::Shr,
    ];

    /// The reflection kinds. The root holds of every kind, ungated by field
    /// visibility (WEP 2026-06-13).
    const REFLECT: [OnBoundTrait; 7] = [
        OnBoundTrait::Reflect,
        OnBoundTrait::ReflectStruct,
        OnBoundTrait::ReflectVariant,
        OnBoundTrait::ReflectEnum,
        OnBoundTrait::ReflectFlags,
        OnBoundTrait::ReflectNewtype,
        OnBoundTrait::ReflectTemplate,
    ];

    pub(crate) fn build(tysys: &TypeSystem, modules: &[ModuleSource]) -> Self {
        let mut lowering = Lowering::over(&tysys.resolutions);
        let mut program = Program::default();
        let table = tysys.type_table.borrow();
        lowering.tuple = table.compiler_item_def(CompilerItem::Tuple);
        program.tuple = lowering.tuple.map(|def| lowering.type_decl(def));
        Self::intern_declarations(tysys, modules, &mut lowering);
        let derivation_sources = Self::derivation_sources(tysys);
        for (&def, &kind) in &derivation_sources {
            if let Some(trait_) = tysys.trait_env.impl_headers[&def].trait_def() {
                let trait_ = lowering.trait_decl(trait_);
                lowering.derivation_source.insert((trait_, kind), def);
            }
        }
        lower_impls(
            &mut lowering,
            &mut program,
            tysys
                .trait_env
                .impl_headers
                .iter()
                .filter(|(def, _)| !derivation_sources.contains_key(*def)),
            &tysys.trait_env,
            &tysys.resolutions,
        );
        Self::state_primitive_impls(tysys, &mut lowering, &mut program);
        mark_duplicates(&mut program);
        Self::state_traits(tysys, &mut lowering, &mut program);
        Self::state_scopes(tysys, modules, &mut lowering, &mut program);
        if let (Some(eq), Some(ord)) = (
            Self::derived_trait(tysys, &mut lowering, CompilerItem::Eq),
            Self::derived_trait(tysys, &mut lowering, CompilerItem::Ord),
        ) {
            pair_comparisons(&mut program, eq, ord);
        }
        let mut bridge = Self { program, lowering };
        let shapes = bridge.shapes(modules);
        bridge.state_declarations(tysys, &tysys.data, &table, shapes, ImplId(0));
        let heads: Vec<TypeDeclId> = (0..bridge.lowering.decls.len())
            .map(|id| TypeDeclId(u32::try_from(id).expect("fewer than 2^32 declarations")))
            .collect();
        bridge.state_ref_facts(tysys, &table, &heads);
        bridge
    }

    /// State the structs and newtypes a block declares, once annotate has
    /// resolved them. One stated already — a body walked again, standing in
    /// another module — is left as it is.
    pub(crate) fn state_local(&mut self, tysys: &TypeSystem, local: &DataDecls, defs: &[DefId]) {
        let pending: Vec<DefId> = defs
            .iter()
            .copied()
            .filter(|&def| {
                self.lowering
                    .unstated
                    .contains(&self.lowering.declared_type(def))
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        let data = local.restricted_to(&pending);
        let table = tysys.type_table.borrow();
        let named_from = self.program.next_impl_id();
        let stated = self.state_declarations(tysys, &data, &table, Vec::new(), named_from);
        self.state_ref_facts(tysys, &table, &stated);
        for head in stated {
            self.lowering.unstated.shift_remove(&head);
        }
    }

    /// The `trait_` a structural item names, interned.
    fn derived_trait(
        tysys: &TypeSystem,
        lowering: &mut Lowering,
        item: CompilerItem,
    ) -> Option<TraitDeclId> {
        Some(lowering.trait_decl(tysys.compiler_trait_def(item)?))
    }

    /// The two shapes no module declares, as `derive` reads them: an anonymous
    /// struct's members are its fields and a template's its holes, each one
    /// tuple argument, so each derives a structural trait where every element
    /// does. Their reflection kinds hold from every module, since a literal's
    /// fields and a template's holes are all visible.
    fn shapes(&self, modules: &[ModuleSource]) -> Vec<(Declaration, OnBoundTrait)> {
        let module = self
            .lowering
            .declared_module(modules.first().expect("a program has a module"));
        [
            (self.lowering.anonymous_head(), OnBoundTrait::ReflectStruct),
            (self.lowering.template_head(), OnBoundTrait::ReflectTemplate),
        ]
        .into_iter()
        .map(|(id, kind)| {
            let shape = Declaration {
                id,
                params: 1,
                variadic: true,
                members: vec![SolverType::Pack(0)],
                module,
            };
            (shape, kind)
        })
        .collect()
    }

    /// State what `data` declares, with `shapes` among its structs: newtype
    /// bases, the impls the declarations derive, their reflection kinds, and
    /// the facts read off their shape. Answers the heads it stated, each a
    /// declaration whose members the lowering can say. The impls from
    /// `named_from` on are `data`'s, a marker among them.
    fn state_declarations(
        &mut self,
        tysys: &TypeSystem,
        data: &DataDecls,
        table: &TypeTable,
        shapes: Vec<(Declaration, OnBoundTrait)>,
        named_from: ImplId,
    ) -> Vec<TypeDeclId> {
        let Self { program, lowering } = self;
        let mut stated = Self::state_newtype_bases(tysys, data, table, lowering, program);
        let (mut members, handles) = Self::declarations(tysys, data, table, lowering);
        let shape_kinds: Vec<(TypeDeclId, OnBoundTrait)> = shapes
            .iter()
            .map(|(shape, kind)| (shape.id, *kind))
            .collect();
        members.extend(shapes.into_iter().map(|(shape, _)| shape));
        stated.extend(members.iter().chain(&handles).map(|decl| decl.id));
        Self::derive_all(tysys, lowering, program, members, handles);
        Self::name_derived_impls(data, lowering, program, named_from);
        Self::state_reflect_facts(tysys, data, &shape_kinds, table, lowering, program);
        Self::state_type_facts(tysys, data, lowering, program);
        stated
    }

    /// The `Reflect*`-bounded value blankets of the structural traits, each
    /// with the reflection kind it bounds on: each is the derived body's source,
    /// which [`derive`] answers for per declaration.
    fn derivation_sources(tysys: &TypeSystem) -> IndexMap<DefId, CompilerItem> {
        let reflect: IndexMap<DefId, CompilerItem> = Self::REFLECT
            .into_iter()
            .filter_map(|kind| {
                let item = kind.compiler_item();
                Some((tysys.compiler_trait_def(item)?, item))
            })
            .collect();
        Self::DERIVED
            .into_iter()
            .filter_map(|item| tysys.compiler_trait_def(item))
            .flat_map(|trait_| {
                tysys
                    .trait_env
                    .blanket_impls
                    .get(&trait_)
                    .into_iter()
                    .flatten()
            })
            .filter(|blanket| blanket.receiver == BlanketReceiver::Value)
            .filter_map(|blanket| {
                let kind = blanket
                    .bounds
                    .iter()
                    .find_map(|bound| reflect.get(&bound.decl()?).copied())?;
                Some((blanket.def, kind))
            })
            .collect()
    }

    /// Name each derived impl, and each marker demanding one, to the blanket
    /// lookup collects for its body: the source of its trait at the
    /// declaration's reflection kind. A marker block declares no method, so
    /// lookup never collects it. A trait the compiler derives without a blanket
    /// (`Eq`, `Ord`) stays unnamed. Only the impls from `first` on are `data`'s.
    fn name_derived_impls(
        data: &DataDecls,
        lowering: &mut Lowering,
        program: &Program,
        first: ImplId,
    ) {
        let kind_of = |key: &DeclKey| match key {
            DeclKey::Def(def) if data.struct_fields.contains_key(def) => {
                Some(CompilerItem::ReflectStruct)
            }
            DeclKey::Def(def) if data.variant_cases.contains_key(def) => {
                Some(CompilerItem::ReflectVariant)
            }
            DeclKey::Def(def) if data.enum_cases.contains_key(def) => {
                Some(CompilerItem::ReflectEnum)
            }
            DeclKey::Def(def) if data.flags_cases.contains_key(def) => {
                Some(CompilerItem::ReflectFlags)
            }
            DeclKey::Def(_) => Some(CompilerItem::ReflectNewtype),
            DeclKey::AnonymousStruct => Some(CompilerItem::ReflectStruct),
            DeclKey::TemplateShape => Some(CompilerItem::ReflectTemplate),
            DeclKey::Builtin(_) | DeclKey::FnShape { .. } | DeclKey::UndeclaredEffect => None,
        };
        for (&id, def) in program.impls.iter().filter(|(id, _)| **id >= first) {
            if !matches!(def.origin, ImplOrigin::Derived | ImplOrigin::Marker) {
                continue;
            }
            let (Some(trait_), SolverType::Decl(head, _)) = (def.trait_, &def.target) else {
                continue;
            };
            let (key, _) = lowering
                .decls
                .get_index(head.0 as usize)
                .expect("a lowered head is interned");
            if let Some(&source) =
                kind_of(key).and_then(|kind| lowering.derivation_source.get(&(trait_, kind)))
            {
                lowering.impl_defs.insert(id, source);
            }
        }
    }

    /// Intern every declaration and module up front, so a query lowers without
    /// interning and a shape nothing lowered is unknown to it.
    fn intern_declarations(tysys: &TypeSystem, modules: &[ModuleSource], lowering: &mut Lowering) {
        for def in tysys.data.declarations() {
            lowering.type_decl(def);
        }
        lowering.anonymous_struct();
        lowering.template_shape();
        let defs = tysys.resolutions.defs();
        lowering.intern_assocs(&tysys.trait_env.trait_decl_headers);
        // A struct or newtype declared in a body has its identity here and its
        // members only once annotate reaches the body.
        for def in defs.iter().filter(|&def| {
            matches!(defs.kind(def), DefKind::Struct | DefKind::Newtype)
                && defs.is_function_local(def)
        }) {
            let head = lowering.type_decl(def);
            lowering.unstated.insert(head);
        }
        for module in modules {
            lowering.module(module);
        }
    }

    /// A primitive carries `Eq`, `Ord` and its operator items without an impl
    /// anyone wrote.
    fn state_primitive_impls(tysys: &TypeSystem, lowering: &mut Lowering, program: &mut Program) {
        let eq_ord: Vec<DefId> = [CompilerItem::Eq, CompilerItem::Ord]
            .into_iter()
            .filter_map(|item| tysys.compiler_trait_def(item))
            .collect();
        let operators: Vec<(CompilerItem, DefId)> = Self::OPERATORS
            .into_iter()
            .filter_map(|op| Some((op, tysys.compiler_trait_def(op)?)))
            .collect();
        for name in PrimitiveType::all_primitive_names() {
            let target = SolverType::Decl(lowering.builtin(name), vec![]);
            let carried = operators
                .iter()
                .filter(|(op, _)| primitive_has_operator(name, *op))
                .map(|(_, def)| *def);
            for trait_ in eq_ord.iter().copied().chain(carried) {
                let trait_ = lowering.trait_decl(trait_);
                // A prelude impl of the trait on the primitive, at any trait
                // arguments, already states it.
                let written = program
                    .impls
                    .values()
                    .any(|def| def.trait_ == Some(trait_) && def.target == target);
                if written {
                    continue;
                }
                program.push_impl(ImplDef {
                    trait_: Some(trait_),
                    trait_args: vec![],
                    target: target.clone(),
                    params: vec![],
                    origin: ImplOrigin::Written,
                });
            }
        }
    }

    /// Each trait's supertraits, argument defaults and reference rule;
    /// `Inspect` holds for all.
    fn state_traits(tysys: &TypeSystem, lowering: &mut Lowering, program: &mut Program) {
        for (trait_, closure) in tysys.trait_env.supertrait_closures_in_own_space() {
            let id = lowering.trait_decl(*trait_);
            let own = tysys.trait_env.trait_type_params(*trait_);
            let place = |name: &str| ParamKind::of(own.iter().copied(), name);
            let space = Written {
                resolutions: &tysys.resolutions,
                param: &place,
                declaring: &|base, assoc| declaring_among(tysys, own.iter().copied(), base, assoc),
                self_type: None,
            };
            // An edge whose arguments the lowering cannot say states nothing:
            // answering it at the supertrait's defaults would be a guess.
            program.traits.entry(id).or_default().supertraits = closure
                .iter()
                .filter(|b| b.via.is_empty())
                .filter_map(|b| {
                    let args = said(
                        b.bound
                            .type_args
                            .iter()
                            .map(|arg| lowering.ast_type(arg, &space)),
                    )
                    .ok()?;
                    Some(ParamBound {
                        trait_: lowering.trait_decl(b.decl),
                        args,
                    })
                })
                .collect();
        }
        if let Some(inspect) = tysys.compiler_trait_def(CompilerItem::Inspect) {
            let id = lowering.trait_decl(inspect);
            program.traits.entry(id).or_default().holds_for_all = true;
        }
        // A reference is itself the thing a `Ref` bound asks for.
        let holds_of_a_reference: Vec<DefId> = [CompilerItem::Ref, CompilerItem::RefMut]
            .into_iter()
            .filter_map(|item| tysys.compiler_trait_def(item))
            .collect();
        // A default naming another parameter is opaque to the solver.
        let closed = Written {
            resolutions: &tysys.resolutions,
            param: &|_| None,
            declaring: &|_, _| None,
            self_type: None,
        };
        for (&trait_, header) in &tysys.trait_env.trait_decl_headers {
            let own = tysys.trait_env.trait_type_params(trait_);
            let defaults: Vec<Option<ArgDefault>> = own
                .iter()
                .map(|p| {
                    p.default.as_ref().map(|default| match default {
                        Type::Named(named) if named.name == "Self" => ArgDefault::SelfType,
                        other => lowering
                            .ast_type(other, &closed)
                            .map_or(ArgDefault::Opaque, ArgDefault::Type),
                    })
                })
                .collect();
            let on_ref = if holds_of_a_reference.contains(&trait_) {
                RefRule::Always
            } else if tysys.ref_denies_bound(tysys.on_bound_of(trait_), trait_) {
                RefRule::Never
            } else {
                RefRule::Inherits
            };
            let (reserved, methods): (Vec<_>, Vec<_>) =
                header.methods.iter().partition(|m| m.is_reserved);
            let methods = methods.iter().map(|m| lowering.method(&m.name)).collect();
            let reserved = reserved.iter().map(|m| lowering.method(&m.name)).collect();
            let id = lowering.trait_decl(trait_);
            let assoc_bounds = header
                .assoc_types
                .iter()
                .map(|assoc| {
                    let bounds = assoc
                        .bounds
                        .iter()
                        .filter_map(|b| tysys.resolutions.bound_decl(b))
                        .map(|def| lowering.trait_decl(def))
                        .collect();
                    (lowering.assoc(id, &assoc.name), bounds)
                })
                .collect();
            let def = program.traits.entry(id).or_default();
            def.arg_defaults = defaults;
            def.pack = own.iter().position(|p| p.is_pack);
            def.on_ref = on_ref;
            def.methods = methods;
            def.reserved = reserved;
            def.assoc_bounds = assoc_bounds;
        }
    }

    /// What each module may name. A trait's methods are candidates at a call
    /// site only where that trait's declaration is in scope there
    /// (WEP 2026-09-01); where its impls were written does not enter.
    fn state_scopes(
        tysys: &TypeSystem,
        modules: &[ModuleSource],
        lowering: &mut Lowering,
        program: &mut Program,
    ) {
        for module in modules {
            // A declaration reachable under two names is in scope once.
            let traits_in_scope: IndexSet<TraitDeclId> = tysys
                .resolutions
                .decls_in_scope(module)
                .into_iter()
                .filter(|def| tysys.trait_env.declares_trait(def))
                .map(|def| lowering.trait_decl(def))
                .collect();
            let id = lowering.module(module);
            program.scopes.insert(
                id,
                ModuleScope {
                    traits_in_scope: traits_in_scope.into_iter().collect(),
                },
            );
        }
    }

    /// A newtype inherits its base's impls, and a `flags` type its primitive's.
    /// Answers the heads whose base it stated.
    fn state_newtype_bases(
        tysys: &TypeSystem,
        data: &DataDecls,
        table: &TypeTable,
        lowering: &mut Lowering,
        program: &mut Program,
    ) -> Vec<TypeDeclId> {
        let u32_ = SolverType::Decl(lowering.builtin(TypeTable::FLAGS_BASE_NAME), vec![]);
        let mut stated = Vec::new();
        let mut newtype_base = |head: TypeDeclId, base: SolverType| {
            program.types.insert(
                head,
                TypeDef {
                    newtype_base: Some(base),
                },
            );
            stated.push(head);
        };
        for (def, base_type) in newtype_decls(data, table) {
            if let Ok(base) = lowering.type_id(table, base_type, &|_| None) {
                newtype_base(lowering.declared_type(def), base);
            }
        }
        for (&def, info) in &data.generic_newtypes {
            let param = |name: &str| ParamKind::of(info.type_params.iter(), name);
            let space = Written {
                resolutions: &tysys.resolutions,
                param: &param,
                declaring: &|base, assoc| {
                    declaring_among(tysys, info.type_params.iter(), base, assoc)
                },
                self_type: None,
            };
            if let Ok(base) = lowering.ast_type(&info.base_type_ast, &space) {
                newtype_base(lowering.declared_type(def), base);
            }
        }
        for &def in data.flags_cases.keys() {
            newtype_base(lowering.declared_type(def), u32_.clone());
        }
        stated
    }

    /// The impls the declarations derive. A handle derives `Eq` alone, so the
    /// handles come last and every other trait stops before them. `Eq`
    /// and `Ord` derive from each other before from the members, by the impls
    /// `pair_comparisons` stated: a written `cmp` gives `Eq`, and a written
    /// `eq` gives no `Ord` (spec-traits.md §Derivation Policy).
    fn derive_all(
        tysys: &TypeSystem,
        lowering: &mut Lowering,
        program: &mut Program,
        mut declarations: Vec<Declaration>,
        handles: Vec<Declaration>,
    ) {
        let handles_from = declarations.len();
        declarations.extend(handles);
        let traits: Vec<(CompilerItem, TraitDeclId)> = Self::DERIVED
            .into_iter()
            .filter_map(|item| Some((item, Self::derived_trait(tysys, lowering, item)?)))
            .collect();
        for &(item, trait_) in &traits {
            let eligible = match item {
                CompilerItem::Eq => &declarations[..],
                CompilerItem::Ord | CompilerItem::Serialize | CompilerItem::Deserialize => {
                    &declarations[..handles_from]
                }
                other => unreachable!("{other:?} is not derived"),
            };
            derive(program, trait_, eligible);
        }
    }

    /// State each declaration's reflection kinds as facts. A struct's kind
    /// holds only from the modules that see every field; each of `shapes`
    /// holds its kind from every module.
    fn state_reflect_facts(
        tysys: &TypeSystem,
        data: &DataDecls,
        shapes: &[(TypeDeclId, OnBoundTrait)],
        table: &TypeTable,
        lowering: &Lowering,
        program: &mut Program,
    ) {
        let kinds: Vec<(TraitDeclId, OnBoundTrait)> = Self::REFLECT
            .into_iter()
            .filter_map(|kind| {
                let def = tysys.compiler_trait_def(kind.compiler_item())?;
                Some((lowering.known_trait(def)?, kind))
            })
            .collect();
        let defs = tysys.resolutions.defs();
        let eligible = |def: DefId| !table.is_sealed_reflect_member(defs.ast_id(def));
        let mut state =
            |head: TypeDeclId, kind: OnBoundTrait, visible_from: Option<Vec<ModuleId>>| {
                for &(trait_, stated) in &kinds {
                    let visible_from = if stated == OnBoundTrait::Reflect {
                        None
                    } else if stated == kind {
                        visible_from.clone()
                    } else {
                        continue;
                    };
                    program.facts.insert((head, trait_), Fact { visible_from });
                }
            };
        for (&def, info) in &data.struct_fields {
            if !eligible(def) {
                continue;
            }
            let visible_from = (!info.fields.is_empty()).then(|| {
                lowering
                    .modules
                    .iter()
                    .filter(|(module, _)| info.fields_visible_from(module))
                    .map(|(_, &id)| ModuleId(id))
                    .collect()
            });
            state(
                lowering.declared_type(def),
                OnBoundTrait::ReflectStruct,
                visible_from,
            );
        }
        for &(head, kind) in shapes {
            state(head, kind, None);
        }
        let of = |kind| move |def: &DefId| (*def, kind);
        let memberless = data
            .variant_cases
            .keys()
            .map(of(OnBoundTrait::ReflectVariant))
            .chain(data.enum_cases.keys().map(of(OnBoundTrait::ReflectEnum)))
            .chain(data.flags_cases.keys().map(of(OnBoundTrait::ReflectFlags)))
            .chain(newtype_decls(data, table).map(|(def, _)| (def, OnBoundTrait::ReflectNewtype)))
            .chain(
                data.generic_newtypes
                    .keys()
                    .map(of(OnBoundTrait::ReflectNewtype)),
            );
        for (def, kind) in memberless {
            if eligible(def) {
                state(lowering.declared_type(def), kind, None);
            }
        }
    }

    /// Two of the things the compiler reads off a type rather than off an
    /// impl: a plain `enum`'s `Display`, and a defaulted struct's `Default`.
    /// Each is a fact stated of a declaration, so it answers for every instance
    /// and from every module.
    fn state_type_facts(
        tysys: &TypeSystem,
        data: &DataDecls,
        lowering: &Lowering,
        program: &mut Program,
    ) {
        let mut fact = |def: DefId, item| {
            if let Some(trait_) = tysys
                .compiler_trait_def(item)
                .and_then(|def| lowering.known_trait(def))
            {
                program.facts.insert(
                    (lowering.declared_type(def), trait_),
                    Fact { visible_from: None },
                );
            }
        };

        // A plain `enum` derives `Display` over the bare case name, so the
        // bound holds before `synthesize_traits` emits the body.
        for &def in data.enum_cases.keys() {
            fact(def, CompilerItem::Display);
        }

        // A struct every one of whose fields has a default derives `Default`
        // from the defaults alone, so the bound holds with no impl written and
        // with no member's own `Default` asked for. A generic one does not:
        // a default is elaborated against the declaration, not an instance.
        for (&def, info) in &data.struct_fields {
            if info.auto_derives_default() {
                fact(def, CompilerItem::Default);
            }
        }
    }

    /// The `Ref` / `RefMut` identities of `heads`: what `is_ref_identity` and
    /// `is_ref_mut_identity` read off a type, stated as facts. Each head is
    /// asked through a type standing for it, so the two paths share the one
    /// predicate; a head standing for no type (a trait, a head whose type is
    /// minted later) states nothing.
    fn state_ref_facts(&mut self, tysys: &TypeSystem, table: &TypeTable, heads: &[TypeDeclId]) {
        let lowering = &self.lowering;
        let trait_of = |item| {
            tysys
                .compiler_trait_def(item)
                .and_then(|def| lowering.known_trait(def))
        };
        let (Some(ref_), Some(ref_mut)) =
            (trait_of(CompilerItem::Ref), trait_of(CompilerItem::RefMut))
        else {
            return;
        };
        let is_variant = |def: DefId| tysys.data.variant_cases.contains_key(&def);
        for &head in heads {
            let (key, _) = lowering
                .decls
                .get_index(head.0 as usize)
                .expect("a stated head is interned");
            let (is_ref, is_ref_mut) = match key {
                DeclKey::Def(_)
                | DeclKey::Builtin(_)
                | DeclKey::FnShape { .. }
                | DeclKey::UndeclaredEffect => {
                    let Some(shape) = representative(tysys, table, lowering.tuple, key) else {
                        continue;
                    };
                    (
                        tysys.is_ref_identity(&shape),
                        tysys.is_ref_mut_identity(&is_variant, &shape),
                    )
                }
                // A literal's shape is a struct, which both predicates read as
                // `Struct { .. }`; none is minted when the program is built.
                // A template shape is a struct too.
                DeclKey::AnonymousStruct | DeclKey::TemplateShape => (true, true),
            };
            for (holds, trait_) in [(is_ref, ref_), (is_ref_mut, ref_mut)] {
                if holds {
                    self.program
                        .facts
                        .insert((head, trait_), Fact { visible_from: None });
                }
            }
        }
    }

    /// Every declaration of `data` as [`derive`] reads it: the structs, plain
    /// enums, flags and variants, and apart from them the unrestricted
    /// resources. One with a member the lowering cannot express is left out.
    fn declarations(
        tysys: &TypeSystem,
        data: &DataDecls,
        table: &TypeTable,
        lowering: &Lowering,
    ) -> (Vec<Declaration>, Vec<Declaration>) {
        let lowered = |def: DefId,
                       params: usize,
                       members: &mut dyn Iterator<Item = TypeId>,
                       module: &ModuleSource|
         -> Option<Declaration> {
            let members = said(members.map(|ty| lowering.type_id(table, ty, &by_position))).ok()?;
            Some(Declaration {
                id: lowering.declared_type(def),
                params: u32::try_from(params).expect("fewer than 2^32 params"),
                variadic: false,
                members,
                module: lowering.declared_module(module),
            })
        };
        let mut out = Vec::new();
        for (&def, info) in &data.struct_fields {
            out.extend(lowered(
                def,
                info.type_param_type_ids.len(),
                &mut info.fields.iter().map(|(_, ty, _)| *ty),
                &info.module_source,
            ));
        }
        let memberless = data
            .enum_cases
            .iter()
            .map(|(&def, info)| (def, &info.module_source))
            .chain(
                data.flags_cases
                    .iter()
                    .map(|(&def, info)| (def, &info.module_source)),
            );
        for (def, module) in memberless {
            out.extend(lowered(def, 0, &mut std::iter::empty(), module));
        }
        for (&def, info) in &data.variant_cases {
            out.extend(lowered(
                def,
                info.type_param_type_ids.len(),
                &mut info
                    .cases
                    .iter()
                    .filter(|c| c.has_payload(table))
                    .map(|c| c.payload),
                &info.module_source,
            ));
        }
        let handles = data
            .resource_types
            .iter()
            .filter(|&(&def, _)| table.is_unrestricted_resource(def))
            .filter_map(|(&def, _)| {
                lowered(
                    def,
                    0,
                    &mut std::iter::empty(),
                    tysys.resolutions.defs().module(def),
                )
            })
            .collect();
        (out, handles)
    }

    /// `type_id` lowered, and the bounds in force around it. Unsaid where a
    /// bound the lowering cannot state is on a parameter this receiver mentions:
    /// its list is short of what the source declares, and answering from a short
    /// list says more than the lowering saw.
    fn env_for(
        &self,
        tysys: &TypeSystem,
        ctx: &scope::Scope,
        type_id: TypeId,
    ) -> Result<(Env, SolverType), Unsaid> {
        // Every parameter in scope takes a position, bounded or not: an
        // unbounded `T` still appears in a receiver such as `Array<T>`, and a
        // receiver the environment cannot place lowers to nothing.
        let place = place_in(ctx);
        let table = tysys.type_table.borrow();
        let param = |name: &str| place(name).map(ParamKind::Type);
        let declaring = |base: &str, assoc: &str| {
            if base == "Self" {
                return ctx
                    .trait_ctx
                    .self_trait
                    .and_then(|trait_| tysys.trait_env.trait_declaring_assoc_type(&trait_, assoc))
                    .or_else(|| {
                        let def = table.nominal_def(ctx.trait_ctx.self_type?)?;
                        let target = ImplTargetKey::of_decl(tysys.resolutions.defs(), def);
                        tysys.trait_env.impl_trait_binding_assoc(&target, assoc)
                    });
            }
            let bounds = ctx.trait_ctx.type_param_bounds.get(base)?;
            tysys
                .trait_env
                .bound_declaring_assoc_type(bounds, assoc, &tysys.resolutions)
        };
        let placed = place_binder(ctx, &table);
        let mut env = Env::default();
        let mut unstated = Vec::new();
        for (position, (name, binder)) in ctx.trait_ctx.type_params.iter().enumerate() {
            // A family parameter also carries what its declaration bounds it
            // by, which an impl writing it need not restate.
            let declared = ctx
                .trait_ctx
                .assoc_param_bounds
                .get(&table.type_key(binder.type_id))
                .into_iter()
                .flatten()
                .map(|declared| self.lowering.bound_named(declared, &param));
            let scoped = ctx
                .trait_ctx
                .type_param_bounds
                .get(name)
                .into_iter()
                .flatten()
                .map(|scoped| {
                    let self_type = scoped
                        .self_binding
                        .filter(|_| scoped.bound.writes_self())
                        .map(|binding| self.lowering.type_id(&table, binding.type_id, &placed))
                        .transpose();
                    // A bound naming no trait was reported where it is written.
                    tysys
                        .resolutions
                        .bound_decl(&scoped.bound)
                        .ok_or(Unsaid::Failed)
                        .and_then(|def| {
                            let self_type = self_type?;
                            let space = Written {
                                resolutions: &tysys.resolutions,
                                param: &param,
                                declaring: &declaring,
                                self_type: self_type.as_ref(),
                            };
                            let args = said(
                                scoped
                                    .bound
                                    .type_args
                                    .iter()
                                    .map(|arg| self.lowering.ast_type(arg, &space)),
                            )?;
                            Ok(ParamBound {
                                trait_: self.lowering.known_trait(def).ok_or(Unsaid::Unsayable)?,
                                args,
                            })
                        })
                });
            let mut ids = Vec::new();
            for stated in declared.chain(scoped) {
                match stated {
                    Ok(bound) => ids.push(bound),
                    Err(unsaid) => unstated.push((position as u32, unsaid)),
                }
            }
            env.param_bounds.push(ids);
        }
        let ty = self.lowering.type_id(&table, type_id, &placed)?;
        said(
            unstated
                .into_iter()
                .filter(|&(position, _)| ty.mentions_param(position))
                .map(|(_, unsaid)| Err(unsaid)),
        )?;
        Ok((env, ty))
    }

    /// Whether `type_id` is one type in `ctx`: no resolution or inference left
    /// it open, and it names no binder `ctx` does not declare, nor a pack,
    /// which stands for many.
    pub(super) fn is_rigid(&self, tysys: &TypeSystem, ctx: &scope::Scope, type_id: TypeId) -> bool {
        let table = tysys.type_table.borrow();
        let place = place_binder(ctx, &table);
        let rigid = |binder: Binder| {
            if let Binder::Param { name, .. } = binder
                && matches!(
                    table.get(ctx.trait_ctx.type_params.get(name)?.type_id),
                    ResolvedType::TypePack { .. }
                )
            {
                return None;
            }
            place(binder)
        };
        matches!(
            self.lowering.type_id(&table, type_id, &rigid),
            Ok(_) | Err(Unsaid::Unsayable)
        )
    }

    /// The question `type_implements_trait` answered, as the solver reads it.
    fn question(
        &self,
        tysys: &TypeSystem,
        ctx: &scope::Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        asked: &FqTraitName,
    ) -> Result<Question, Unsaid> {
        let (env, ty) = self.env_for(tysys, ctx, type_id)?;
        if ty.mentions_decl(&|h| self.lowering.unstated.contains(&h)) {
            return Err(Unsaid::Unsayable);
        }
        let module = self
            .lowering
            .known_module(scope.current_module_source)
            .ok_or(Unsaid::Unsayable)?;
        let place = place_in(ctx);
        let ParamBound { trait_, args } = self
            .lowering
            .bound_named(asked, &|name| place(name).map(ParamKind::Type))?;
        Ok(Question {
            env,
            ty,
            trait_,
            module,
            args,
        })
    }

    /// Whether `type_id` satisfies `asked`, with the bodies the answer owes,
    /// each keyed as synthesis keys one: the head, its module, and the trait.
    /// `None` where it does not hold.
    pub(super) fn answer_owing(
        &self,
        tysys: &TypeSystem,
        ctx: &scope::Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        asked: &FqTraitName,
    ) -> Option<Vec<OwedBody>> {
        let q = match self.question(tysys, ctx, scope, type_id, asked) {
            Ok(q) => q,
            // Its failure was reported where it failed, so it meets every
            // bound rather than failing them over again.
            Err(Unsaid::Failed) => return Some(Vec::new()),
            // No impl answers for a type still open, so it holds what every
            // type holds.
            Err(Unsaid::Open) => {
                let holds = asked
                    .canonical()
                    .and_then(|decl| self.lowering.known_trait(decl))
                    .and_then(|trait_| self.program.traits.get(&trait_))
                    .is_some_and(|def| def.holds_for_all);
                return holds.then(Vec::new);
            }
            Err(Unsaid::Unsayable) => panic!(
                "the lowering states no `{}: {asked}`",
                tysys.type_table.borrow().type_name(type_id)
            ),
        };
        let held = holds_with_args(&self.program, &q.env, &q.ty, q.trait_, q.module, &q.args)?;
        let owed = owed(&self.program, &q.env, q.module, held.requests);
        let table = tysys.type_table.borrow();
        let shape_heads = [
            self.lowering.anonymous_head(),
            self.lowering.template_head(),
        ];
        let mut shapes = Vec::new();
        if owed
            .iter()
            .any(|r| matches!(&r.ty, SolverType::Decl(head, _) if shape_heads.contains(head)))
        {
            self.shapes_in(&table, type_id, &place_binder(ctx, &table), &mut shapes);
        }
        let defs = tysys.resolutions.defs();
        let bodies = owed
            .into_iter()
            .flat_map(|request| {
                let trait_ = self.lowering.trait_def_of(request.trait_);
                let SolverType::Decl(head, _) = &request.ty else {
                    return Vec::new();
                };
                let (key, _) = self
                    .lowering
                    .decls
                    .get_index(head.0 as usize)
                    .expect("a lowered head is interned");
                match key {
                    DeclKey::Def(def) => vec![OwedBody {
                        head: FqTypeName::declared(defs, *def).head().clone(),
                        module: table.def_module(*def).clone(),
                        trait_,
                    }],
                    // Two shapes of one field-type list lower alike, so each
                    // is owed the body.
                    DeclKey::AnonymousStruct | DeclKey::TemplateShape => shapes
                        .iter()
                        .filter(|(lowered, _)| *lowered == request.ty)
                        .map(|&(_, shape)| OwedBody {
                            head: table.fq_struct_head(StructDef::Anon(shape)).head().clone(),
                            module: table.anon_struct_module(shape).clone(),
                            trait_,
                        })
                        .collect(),
                    DeclKey::Builtin(_) | DeclKey::FnShape { .. } | DeclKey::UndeclaredEffect => {
                        Vec::new()
                    }
                }
            })
            .collect();
        Some(bodies)
    }

    /// Each anonymous shape `id` is built over, with its lowering: what a
    /// body owed at a shape names it by.
    fn shapes_in(
        &self,
        table: &TypeTable,
        id: TypeId,
        param: &dyn Fn(Binder) -> Option<u32>,
        out: &mut Vec<(SolverType, AnonStructId)>,
    ) {
        let mut each = |ids: &[TypeId]| {
            for &inner in ids {
                self.shapes_in(table, inner, param, out);
            }
        };
        match table.get(id) {
            ResolvedType::Struct {
                def: StructDef::Anon(shape),
                ..
            } => {
                let shape = *shape;
                let members: Vec<TypeId> = match table.template_shape(shape) {
                    Some(template) => template.holes.iter().map(|hole| hole.ty).collect(),
                    None => table
                        .anon_struct_fields(shape)
                        .iter()
                        .map(|&(_, ty)| ty)
                        .collect(),
                };
                each(&members);
                if let Ok(lowered) = self.lowering.type_id(table, id, param) {
                    out.push((lowered, shape));
                }
            }
            ResolvedType::Struct {
                def: StructDef::Decl(_),
                type_args,
            }
            | ResolvedType::GenericInstance { type_args, .. }
            | ResolvedType::GenericResource { type_args, .. }
            | ResolvedType::Newtype { type_args, .. } => each(type_args),
            ResolvedType::BuiltinArray(inner)
            | ResolvedType::Ref(inner)
            | ResolvedType::MutRef(inner) => each(&[*inner]),
            ResolvedType::Function {
                params,
                return_type,
                ..
            } => {
                each(params);
                each(&[*return_type]);
            }
            _ => {}
        }
    }

    /// The impls the order ties for a bound [`Self::answer_owing`] holds: a
    /// bound reaches them as a call does, so neither may win by declaration
    /// order.
    pub(super) fn tied_through_bound(
        &self,
        tysys: &TypeSystem,
        ctx: &scope::Scope,
        scope: &TypeLookup,
        type_id: TypeId,
        asked: &FqTraitName,
    ) -> Vec<Option<DefId>> {
        let Ok(q) = self.question(tysys, ctx, scope, type_id, asked) else {
            return Vec::new();
        };
        let found = bound_candidates(&self.program, &q.env, &q.ty, q.trait_, q.module, &q.args);
        match rank(&found) {
            Selection::AmbiguousBlankets(live) => live
                .iter()
                .map(|&i| self.impl_def_of(found[i].impl_))
                .collect(),
            Selection::None
            | Selection::One(_)
            | Selection::AmbiguousTraits(_)
            | Selection::Overloaded(_)
            | Selection::Duplicated(_) => Vec::new(),
        }
    }

    /// What the order selects for a call of `method_name` on `type_id` made in
    /// `module`; `None` where the question is outside what the lowering states.
    ///
    /// A receiver mentioning a type parameter is one of those: the bounds in
    /// force at the call site reach here as an `Env`, and the selection path
    /// carries no annotate-time scope to build one from. Such a call is skipped
    /// rather than answered wrongly.
    pub(super) fn select(
        &self,
        tysys: &TypeSystem,
        ctx: &scope::Scope,
        module: &ModuleSource,
        type_id: TypeId,
        through_ref: Option<bool>,
        method_name: &str,
        required_trait: Option<DefId>,
    ) -> Option<Ordered> {
        let method = MethodId(*self.lowering.methods.get(method_name)?);
        // A trait-qualified call named its trait, so the order runs within it:
        // every rank still applies, and the cross-trait question does not.
        let required = match required_trait {
            Some(def) => Some(self.lowering.known_trait(def)?),
            None => None,
        };
        let (env, ty) = self.env_for(tysys, ctx, type_id).ok()?;
        let ty = match through_ref {
            Some(is_mut) => SolverType::Ref {
                is_mut,
                inner: Box::new(ty),
            },
            None => ty,
        };
        let scope = self.lowering.known_module(module)?;
        let mut found = candidates(&self.program, &env, &ty, method, scope);
        // An out-of-scope candidate suggests importing its trait, so a trait the
        // call site cannot name is dropped and the call reads as a missing method.
        let defs = tysys.resolutions.defs();
        found
            .out_of_scope
            .retain(|c| defs.nameable_from(self.lowering.trait_def_of(c.trait_), module));
        // Both sets: an out-of-scope candidate of another trait would otherwise
        // stand in for the named one, asking for an import that cannot make the
        // named trait apply.
        if let Some(required) = required {
            found.in_scope.retain(|c| c.trait_ == required);
            found.out_of_scope.retain(|c| c.trait_ == required);
        }
        // The caller asks a reference receiver in two passes — its `&T` impls
        // first, the pointee's after (`method_call.rs`) — so the reference pass
        // is answered from the reference level alone.
        if through_ref.is_some() {
            let on_ref = |c: &Candidate| {
                matches!(self.program.impls[&c.impl_].target, SolverType::Ref { .. })
            };
            found.in_scope.retain(on_ref);
            found.out_of_scope.retain(on_ref);
        }
        let named = |live: &[usize]| {
            live.iter()
                .map(|&i| self.impl_def_of(found.in_scope[i].impl_))
                .collect()
        };
        Some(match rank(&found.in_scope) {
            Selection::One(index) => Ordered::One(self.impl_def_of(found.in_scope[index].impl_)),
            Selection::None if !found.out_of_scope.is_empty() => {
                let mut traits: Vec<DefId> = Vec::new();
                for c in &found.out_of_scope {
                    let trait_ = self.lowering.trait_def_of(c.trait_);
                    if !traits.contains(&trait_) {
                        traits.push(trait_);
                    }
                }
                Ordered::OutOfScope {
                    traits,
                    impls: found
                        .out_of_scope
                        .iter()
                        .map(|c| self.impl_def_of(c.impl_))
                        .collect(),
                }
            }
            Selection::None => Ordered::Nothing,
            Selection::AmbiguousTraits(live) => Ordered::AmbiguousTraits(named(&live)),
            Selection::AmbiguousBlankets(live) => Ordered::AmbiguousBlankets(named(&live)),
            // Coherence rejects these where they are written, so the order has
            // nothing to add and the caller keeps whichever it collected.
            Selection::Duplicated(live) => Ordered::Duplicated(named(&live)),
            // One trait at several argument lists is the call's arguments to
            // settle (WEP 2026-07-31), which the order does not answer.
            Selection::Overloaded(live) => Ordered::Overloaded(named(&live)),
        })
    }

    /// Which of `Eq` and `Ord` is written for `instance` without the other,
    /// with the block writing it: the row of the comparison table `instance`
    /// reads, by [`comparison_row`]. A type parameter is rigid, so only an impl
    /// reaching every instance reaches it.
    pub(crate) fn comparison_written_alone(
        &self,
        table: &TypeTable,
        instance: TypeId,
    ) -> Option<(CompilerItem, DefId)> {
        let (eq, _) = self.program.comparisons?;
        // A shape the lowering cannot say is one no source names, so no written
        // impl reaches it.
        let ty = self.lowering.type_id(table, instance, &by_position).ok()?;
        let (written, impl_) = comparison_row(&self.program, &ty)?;
        let item = if written == eq {
            CompilerItem::Eq
        } else {
            CompilerItem::Ord
        };
        Some((
            item,
            self.impl_def_of(impl_)
                .expect("a written impl names its block"),
        ))
    }

    /// Whether a written block applies at `instance`, its bounds included: what
    /// a generic body's instance selects by, every parameter settled. Where the
    /// lowering states nothing about the block or the instance (a closure
    /// environment, or a template's own `Self` whose parameters no bound in
    /// scope here answers), the target match alone decides.
    pub(crate) fn blocks_applying_at<'a>(
        &'a self,
        table: &'a TypeTable,
        instance: TypeId,
    ) -> impl Fn(DefId) -> bool + 'a {
        let ty = self.lowering.type_id(table, instance, &|_| None).ok();
        move |block| {
            table.impl_reaches_instance(block, instance)
                && self
                    .block_applies(table, block, ty.as_ref())
                    .is_none_or(|applies| applies)
        }
    }

    fn block_applies(
        &self,
        table: &TypeTable,
        block: DefId,
        ty: Option<&SolverType>,
    ) -> Option<bool> {
        let impl_ = *self.lowering.written_impls.get(&block)?;
        let scope = self.lowering.known_module(table.def_module(block))?;
        Some(applies(&self.program, &Env::default(), scope, impl_, ty?))
    }

    /// The impl block a candidate names: the one it was lowered from, or for a
    /// derived body the `Reflect*` blanket lookup collects for it. `None` for a
    /// body the compiler supplies with no block at all, which is how a
    /// `TraitMethodMatch` says it too.
    fn impl_def_of(&self, impl_: ImplId) -> Option<DefId> {
        self.lowering.impl_defs.get(&impl_).copied()
    }
}

/// What the order answers, naming each candidate by the impl block it came
/// from — `None` where a derived body answers and no block was written, which
/// is how a [`TraitMethodMatch`](super::types::TraitMethodMatch) says it too.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Ordered {
    /// Exactly one impl answers.
    One(Option<DefId>),
    /// No impl applied at all.
    Nothing,
    /// Impls applied and the call site had imported none of their traits. The
    /// scope gate working, not a candidate lost: the message names the traits
    /// an import would choose between, and one of the impls stands in so the
    /// call does not also read as a missing method.
    OutOfScope {
        traits: Vec<DefId>,
        impls: Vec<Option<DefId>>,
    },
    /// Several trait declarations declare the method; the call must name one.
    AmbiguousTraits(Vec<Option<DefId>>),
    /// Several impls of one trait, none written for the receiver.
    AmbiguousBlankets(Vec<Option<DefId>>),
    /// One trait at several argument lists — the call's arguments choose.
    Overloaded(Vec<Option<DefId>>),
    /// Several impls written for the receiver at one argument list, kept apart
    /// only by bounds that all hold here.
    Duplicated(Vec<Option<DefId>>),
}

/// Where each type parameter `ctx` has in scope sits in the environment
/// [`SolverBridge::env_for`] built, which is what gives a rigid parameter its
/// [`SolverType::Param`].
fn place_in(ctx: &scope::Scope) -> impl Fn(&str) -> Option<u32> + '_ {
    |name: &str| {
        ctx.trait_ctx
            .type_params
            .get_index_of(name)
            .map(|i| u32::try_from(i).expect("fewer than 2^32 params"))
    }
}

/// A type parameter at its declared position, where no scope places binders:
/// a declaration's members, or a type read with its parameters rigid. A family
/// parameter has no declared position.
fn by_position(binder: Binder) -> Option<u32> {
    match binder {
        Binder::Param { index, .. } => Some(index),
        Binder::Family(_) => None,
    }
}

/// [`place_in`] for a binder a resolved type names. A family parameter sits
/// where the scope binds its identity, under whatever name it is written.
fn place_binder<'a>(
    ctx: &'a scope::Scope,
    table: &'a TypeTable,
) -> impl Fn(Binder) -> Option<u32> + 'a {
    let place = place_in(ctx);
    move |binder| match binder {
        Binder::Param { name, .. } => place(name),
        Binder::Family(id) => {
            let key = table.type_key(id);
            ctx.trait_ctx
                .type_params
                .values()
                .position(|bound| table.type_key(bound.type_id) == key)
                .map(|i| u32::try_from(i).expect("fewer than 2^32 params"))
        }
    }
}

/// A body an answer owes, keyed as synthesis keys the request for it.
pub(super) struct OwedBody {
    pub(super) head: TypeHead,
    pub(super) module: ModuleSource,
    pub(super) trait_: DefId,
}

/// A `type_implements_trait` question as the solver reads it.
struct Question {
    env: Env,
    ty: SolverType,
    trait_: TraitDeclId,
    module: ModuleId,
    /// The arguments the asking bound writes for the trait's own parameters.
    args: Vec<SolverType>,
}
