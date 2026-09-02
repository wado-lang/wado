//! The lowering from the compiler's tables into the solver's [`Program`], and
//! the differential that holds the two answers together while the compiler's
//! own path is still authoritative.
//!
//! This is the one place a `DefId`, a `TypeId`, an AST type or a spelling
//! becomes a plain index. Nothing in `trait_solver` knows any of them.

use crate::ast::Type;
use crate::defs::DefId;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::is_builtin_shape_name;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::trait_solver::{
    ArgDefault, AssocId, Declaration, Env, Fact, ImplDef, ImplOrigin, ModuleId, ParamDef, Pin,
    Program, RefRule, SolverType, TraitDeclId, TraitDef, TypeDeclId, TypeDef, derive, holds,
};

use super::trait_env::ImplHeader;
use super::tysys::TypeSystem;

/// What a [`TypeDeclId`] stands for: a declaration, or a shape no module
/// declares — a primitive, which the compiler answers for by name.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
enum DeclKey {
    Def(DefId),
    Builtin(String),
}

/// How an impl's parameter is spelled where a type mentions it.
#[derive(Clone, Copy)]
pub(super) enum ParamKind {
    Type(u32),
    Pack(u32),
}

/// The interning both directions share, so an impl lowered from its header
/// and a receiver lowered from the type table name one declaration by one id.
#[derive(Default)]
pub(super) struct Lowering {
    decls: IndexMap<DeclKey, u32>,
    modules: IndexMap<ModuleSource, u32>,
    /// The declaration a tuple type is an instance of. An impl writes a tuple
    /// as `[..T]`, so an instance lowers to [`SolverType::Tuple`] as well.
    tuple: Option<DefId>,
    /// A trait's associated types, by the trait and the name.
    assocs: IndexMap<(TraitDeclId, String), u32>,
}

impl Lowering {
    fn intern(&mut self, key: DeclKey) -> u32 {
        let next =
            u32::try_from(self.decls.len()).expect("a program declares fewer than 2^32 items");
        *self.decls.entry(key).or_insert(next)
    }

    fn type_decl(&mut self, def: DefId) -> TypeDeclId {
        TypeDeclId(self.intern(DeclKey::Def(def)))
    }

    fn builtin(&mut self, name: &str) -> TypeDeclId {
        TypeDeclId(self.intern(DeclKey::Builtin(name.to_string())))
    }

    pub(super) fn trait_decl(&mut self, def: DefId) -> TraitDeclId {
        TraitDeclId(self.intern(DeclKey::Def(def)))
    }

    fn assoc(&mut self, trait_: TraitDeclId, name: &str) -> AssocId {
        let next = u32::try_from(self.assocs.len()).expect("fewer than 2^32 associated types");
        AssocId(
            *self
                .assocs
                .entry((trait_, name.to_string()))
                .or_insert(next),
        )
    }

    fn module(&mut self, module: &ModuleSource) -> ModuleId {
        let next = u32::try_from(self.modules.len()).expect("fewer than 2^32 modules");
        ModuleId(*self.modules.entry(module.clone()).or_insert(next))
    }

    /// Read-only: the id a declaration was given, if it was ever lowered. A
    /// query on a shape nothing lowered has nothing to disagree about.
    fn known_trait(&self, def: DefId) -> Option<TraitDeclId> {
        self.decls.get(&DeclKey::Def(def)).map(|&i| TraitDeclId(i))
    }

    fn known_type(&self, key: &DeclKey) -> Option<TypeDeclId> {
        self.decls.get(key).map(|&i| TypeDeclId(i))
    }

    fn known_module(&self, module: &ModuleSource) -> Option<ModuleId> {
        self.modules.get(module).map(|&i| ModuleId(i))
    }

    /// One AST type as the solver reads it, or `None` for a shape it has no way
    /// to say. `param` says which names are the surrounding item's own
    /// parameters, so a name among them is a position rather than a
    /// declaration; `self_type` is what `Self` means here, if anything.
    pub(super) fn ast_type(
        &mut self,
        ty: &Type,
        param: &dyn Fn(&str) -> Option<ParamKind>,
        resolutions: &crate::resolve::Resolutions,
        self_type: Option<&SolverType>,
    ) -> Option<SolverType> {
        // A builtin shape — a primitive, `()`, `Array` — is keyed by its
        // spelling on both sides, as `ImplTargetKey::of_decl` keys it: the
        // type table answers for it by name, not by the declaration it has.
        match ty {
            Type::Named(named) if named.name == "Self" => self_type.cloned(),
            Type::Named(named) => match param(&named.name) {
                Some(ParamKind::Type(index)) => Some(SolverType::Param(index)),
                Some(ParamKind::Pack(index)) => Some(SolverType::Pack(index)),
                None if is_builtin_shape_name(&named.name) => {
                    Some(SolverType::Decl(self.builtin(&named.name), Vec::new()))
                }
                None => resolutions
                    .declared(named.id)
                    .map(|def| SolverType::Decl(self.type_decl(def), Vec::new())),
            },
            Type::Generic(generic) => {
                // A generic head is a declaration; an impl parameter never
                // carries type arguments of its own.
                let head = if is_builtin_shape_name(&generic.name) {
                    self.builtin(&generic.name)
                } else {
                    self.type_decl(resolutions.declared(generic.id)?)
                };
                let mut args = Vec::with_capacity(generic.args.len());
                for arg in &generic.args {
                    args.push(self.ast_type(arg, param, resolutions, self_type)?);
                }
                Some(SolverType::Decl(head, args))
            }
            Type::Tuple(elems) if elems.is_empty() => Some(SolverType::Decl(
                self.builtin(TypeTable::UNIT_TYPE_NAME),
                Vec::new(),
            )),
            Type::Tuple(elems) => {
                let mut lowered = Vec::with_capacity(elems.len());
                for elem in elems {
                    lowered.push(self.ast_type(elem, param, resolutions, self_type)?);
                }
                Some(SolverType::Tuple(lowered))
            }
            Type::TypePackSpread(name, _) => match param(name)? {
                ParamKind::Pack(index) | ParamKind::Type(index) => Some(SolverType::Pack(index)),
            },
            Type::Reference(inner) => Some(SolverType::Ref {
                is_mut: false,
                inner: Box::new(self.ast_type(inner, param, resolutions, self_type)?),
            }),
            Type::MutReference(inner) => Some(SolverType::Ref {
                is_mut: true,
                inner: Box::new(self.ast_type(inner, param, resolutions, self_type)?),
            }),
            Type::NamespacedGeneric(_) | Type::Function(_) | Type::Infer(_) | Type::Error(_) => {
                None
            }
        }
    }

    /// One resolved type as the solver reads it, without interning anything:
    /// a declaration nothing lowered is a shape no impl can name, and the
    /// caller decides what that means. `param` maps a rigid type parameter to
    /// the position the caller's environment gives it.
    fn type_id(
        &self,
        table: &TypeTable,
        id: TypeId,
        param: &dyn Fn(&str, u32) -> Option<u32>,
    ) -> Option<SolverType> {
        let decl = |key: DeclKey, args: Vec<SolverType>| {
            self.known_type(&key).map(|id| SolverType::Decl(id, args))
        };
        let lower_args = |args: &[TypeId]| -> Option<Vec<SolverType>> {
            args.iter()
                .map(|&a| self.type_id(table, a, param))
                .collect()
        };
        match table.get(id) {
            ResolvedType::Primitive(p) => decl(DeclKey::Builtin(p.as_str().to_string()), vec![]),
            ResolvedType::BuiltinArray(elem) => decl(
                DeclKey::Builtin(TypeTable::ARRAY_TYPE_NAME.to_string()),
                vec![self.type_id(table, *elem, param)?],
            ),
            // `()` is the unit type, not the empty tuple `[]`: an
            // `impl Tr for ()` is its own, and `[..T]` does not answer for it
            // (WEP 2026-09-01, "The candidates").
            ResolvedType::Unit => decl(DeclKey::Builtin(TypeTable::UNIT_TYPE_NAME.into()), vec![]),
            ResolvedType::Struct { def, type_args } => {
                let def = def.decl()?;
                if self.tuple == Some(def) {
                    return Some(SolverType::Tuple(lower_args(type_args)?));
                }
                decl(DeclKey::Def(def), lower_args(type_args)?)
            }
            ResolvedType::Enum { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Flags { def } => decl(DeclKey::Def(*def), vec![]),
            ResolvedType::GenericResource { def, type_args }
            | ResolvedType::GenericInstance { def, type_args }
            | ResolvedType::Newtype { def, type_args, .. } => {
                if self.tuple == Some(*def) {
                    return Some(SolverType::Tuple(lower_args(type_args)?));
                }
                decl(DeclKey::Def(*def), lower_args(type_args)?)
            }
            ResolvedType::Ref(inner) => Some(SolverType::Ref {
                is_mut: false,
                inner: Box::new(self.type_id(table, *inner, param)?),
            }),
            ResolvedType::MutRef(inner) => Some(SolverType::Ref {
                is_mut: true,
                inner: Box::new(self.type_id(table, *inner, param)?),
            }),
            ResolvedType::TypeParam { name, index } => param(name, *index).map(SolverType::Param),
            ResolvedType::Never
            | ResolvedType::Function { .. }
            | ResolvedType::Reactive(_)
            | ResolvedType::InferVar(_)
            | ResolvedType::TypePack { .. }
            | ResolvedType::AssocTypeProjection { .. }
            | ResolvedType::Unknown
            | ResolvedType::Error => None,
        }
    }
}

/// Lower the impl headers into the value the solver answers from, and hand
/// back the header each [`ImplId`] stands for so a finding can be given a span.
///
/// A header whose target the lowering cannot express is dropped rather than
/// approximated: an approximate target would key against the wrong impls, and
/// the shapes dropped here — a function type, an associated-type projection, an
/// unresolved name — carry a diagnostic of their own already.
pub(super) fn lower_impls(
    lowering: &mut Lowering,
    program: &mut Program,
    impl_headers: &IndexMap<DefId, ImplHeader>,
    resolutions: &crate::resolve::Resolutions,
) -> Vec<DefId> {
    let mut sources: Vec<DefId> = Vec::new();
    for (&impl_def, header) in impl_headers {
        // A name among the target's arguments that no declaration answers is
        // one of the impl's own parameters, declared or not:
        // `impl FromIterator for List<T>` binds `T` without an `impl<T>`
        // (the rule `build_declared_type_params` reads the same way). The
        // declared ones keep their positions; the implicit ones follow.
        let implicit: Vec<&str> = match &header.ty {
            Type::Generic(generic) => generic
                .args
                .iter()
                .filter_map(|arg| match arg {
                    Type::Named(named)
                        if !header.type_params.iter().any(|p| p.name == named.name)
                            && resolutions.declared(named.id).is_none()
                            && !PrimitiveType::is_primitive_name(&named.name) =>
                    {
                        Some(named.name.as_str())
                    }
                    Type::Named(_)
                    | Type::Generic(_)
                    | Type::NamespacedGeneric(_)
                    | Type::Function(_)
                    | Type::Tuple(_)
                    | Type::Reference(_)
                    | Type::MutReference(_)
                    | Type::TypePackSpread(_, _)
                    | Type::Infer(_)
                    | Type::Error(_) => None,
                })
                .collect(),
            Type::Named(_)
            | Type::NamespacedGeneric(_)
            | Type::Function(_)
            | Type::Tuple(_)
            | Type::Reference(_)
            | Type::MutReference(_)
            | Type::TypePackSpread(_, _)
            | Type::Infer(_)
            | Type::Error(_) => Vec::new(),
        };
        let declared = header.type_params.len();
        let param = |name: &str| -> Option<ParamKind> {
            let index =
                |i: usize| u32::try_from(i).expect("an impl declares fewer than 2^32 params");
            if let Some(i) = header.type_params.iter().position(|p| p.name == name) {
                return Some(if header.type_params[i].is_pack {
                    ParamKind::Pack(index(i))
                } else {
                    ParamKind::Type(index(i))
                });
            }
            implicit
                .iter()
                .position(|n| *n == name)
                .map(|i| ParamKind::Type(index(declared + i)))
        };
        let Some(target) = lowering.ast_type(&header.ty, &param, resolutions, None) else {
            continue;
        };
        let trait_args = match header.trait_type.as_ref() {
            Some(Type::Generic(generic)) => {
                let mut args = Vec::with_capacity(generic.args.len());
                for arg in &generic.args {
                    let Some(arg) = lowering.ast_type(arg, &param, resolutions, Some(&target))
                    else {
                        break;
                    };
                    args.push(arg);
                }
                // A partially lowered argument list would key against impls it
                // does not name, so the header is dropped as its target would be.
                if args.len() != generic.args.len() {
                    continue;
                }
                args
            }
            Some(
                Type::Named(_)
                | Type::NamespacedGeneric(_)
                | Type::Function(_)
                | Type::Tuple(_)
                | Type::Reference(_)
                | Type::MutReference(_)
                | Type::TypePackSpread(_, _)
                | Type::Infer(_)
                | Type::Error(_),
            )
            | None => Vec::new(),
        };
        let params = header
            .type_params
            .iter()
            .map(|p| {
                let mut def = ParamDef::default();
                for b in &p.bounds {
                    let Some(bound) = b.resolved.or_else(|| resolutions.declared(b.id)) else {
                        continue;
                    };
                    let bound = lowering.trait_decl(bound);
                    def.bounds.push(bound);
                    // `T: Mul<Output = T>`. A pin the lowering cannot spell
                    // is not carried, which is what the compiler's own check
                    // does with a pin to anything but the receiver.
                    for constraint in &b.assoc_types {
                        let Some(ty) =
                            lowering.ast_type(&constraint.ty, &param, resolutions, Some(&target))
                        else {
                            continue;
                        };
                        def.pins.push(Pin {
                            trait_: bound,
                            assoc: lowering.assoc(bound, &constraint.name),
                            ty,
                        });
                    }
                }
                def
            })
            .chain(implicit.iter().map(|_| ParamDef::default()))
            .collect();
        let id = program.next_impl_id();
        if let Some(implemented) = header.trait_ref {
            let implemented = lowering.trait_decl(implemented);
            for binding in &header.associated_types {
                let Some(ty) = lowering.ast_type(&binding.ty, &param, resolutions, Some(&target))
                else {
                    continue;
                };
                program
                    .assoc_bindings
                    .insert((id, lowering.assoc(implemented, &binding.name)), ty);
            }
        }
        program.add_impl(
            id,
            ImplDef {
                trait_: header.trait_ref.map(|t| lowering.trait_decl(t)),
                trait_args,
                target,
                params,
                origin: if header.is_synthesize_request {
                    ImplOrigin::Marker
                } else {
                    ImplOrigin::Written
                },
            },
        );
        sources.push(impl_def);
    }
    sources
}

/// The shape a reflection kind holds of.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ReflectKind {
    Any,
    Struct,
    Variant,
    Enum,
    Flags,
}

/// The solver's view of the whole program, built once every module's
/// declarations are resolved and before any body is, and the differential that
/// checks its answers against the path in use.
pub(crate) struct SolverBridge {
    program: Program,
    lowering: Lowering,
    /// Traits the differential does not ask about: the compiler items whose
    /// answers the lowering does not state yet — the reference identities,
    /// `Display`, `Default`.
    excluded: IndexSet<DefId>,
}

impl SolverBridge {
    /// The four structural traits [`derive`] generates impls for.
    const DERIVED: [crate::compiler_item::CompilerItem; 4] = [
        crate::compiler_item::CompilerItem::Eq,
        crate::compiler_item::CompilerItem::Ord,
        crate::compiler_item::CompilerItem::Serialize,
        crate::compiler_item::CompilerItem::Deserialize,
    ];

    pub(crate) fn build(tysys: &TypeSystem) -> Self {
        let mut lowering = Lowering::default();
        let mut program = Program::new();
        let resolutions = &tysys.resolutions;
        let table = tysys.type_table.borrow();
        lowering.tuple = table.compiler_item_def(crate::compiler_item::CompilerItem::Tuple);

        // Every declaration and primitive gets an id up front, so a receiver
        // lowered at a query names the same id an impl did — and one that was
        // never lowered is a shape nothing can implement.
        for def in tysys
            .all_struct_fields
            .keys()
            .chain(tysys.all_variant_cases.keys())
            .chain(tysys.all_enum_cases.keys())
            .chain(tysys.all_flags_cases.keys())
            .chain(tysys.all_newtypes.keys())
            .chain(tysys.all_generic_newtypes.keys())
            .chain(tysys.all_resource_types.keys())
        {
            lowering.type_decl(*def);
        }
        // Every module too: a fact names the modules it is visible from, and
        // a question is asked from one that may declare no type at all.
        for module in tysys.module_visible_types.keys().chain(
            resolutions
                .defs()
                .iter()
                .map(|def| resolutions.defs().module(def)),
        ) {
            lowering.module(module);
        }

        lower_impls(
            &mut lowering,
            &mut program,
            &tysys.trait_env.impl_headers,
            resolutions,
        );

        // A primitive carries `Eq`, `Ord` and its operator items without an
        // impl anyone wrote.
        let eq = tysys.compiler_trait_def(crate::compiler_item::CompilerItem::Eq);
        let ord = tysys.compiler_trait_def(crate::compiler_item::CompilerItem::Ord);
        for primitive in PrimitiveType::ALL {
            let target = SolverType::Decl(lowering.builtin(primitive.as_str()), vec![]);
            let operators = Self::OPERATORS
                .into_iter()
                .filter(|&op| super::trait_query::primitive_has_operator(primitive.as_str(), op))
                .filter_map(|op| tysys.compiler_trait_def(op));
            for trait_ in [eq, ord].into_iter().flatten().chain(operators) {
                let id = program.next_impl_id();
                program.add_impl(
                    id,
                    ImplDef {
                        trait_: Some(lowering.trait_decl(trait_)),
                        trait_args: vec![],
                        target: target.clone(),
                        params: vec![],
                        origin: ImplOrigin::Written,
                    },
                );
            }
        }

        for (trait_, closure) in tysys.trait_env.supertrait_closures() {
            let id = lowering.trait_decl(*trait_);
            program.traits.insert(
                id,
                TraitDef {
                    supertraits: closure
                        .iter()
                        .map(|b| lowering.trait_decl(b.decl))
                        .collect(),
                    ..TraitDef::default()
                },
            );
        }
        if let Some(inspect) = tysys.compiler_trait_def(crate::compiler_item::CompilerItem::Inspect)
        {
            let id = lowering.trait_decl(inspect);
            program.traits.entry(id).or_default().holds_for_all = true;
        }
        for (&trait_, header) in &tysys.trait_env.trait_decl_headers {
            let defaults: Vec<Option<ArgDefault>> = header
                .type_params
                .iter()
                .map(|p| {
                    p.default.as_ref().map(|default| match default {
                        Type::Named(named) if named.name == "Self" => ArgDefault::SelfType,
                        other => lowering
                            .ast_type(other, &|_| None, resolutions, None)
                            .map_or(ArgDefault::Opaque, ArgDefault::Type),
                    })
                })
                .collect();
            // `&T` compares by identity, so it is `Eq` of itself; it inherits
            // the rest from `T` by auto-deref, where the trait's own shape
            // lets a reference forward to the pointee.
            let on_ref = if Some(trait_) == eq {
                RefRule::Always
            } else if tysys.ref_denies_bound(tysys.on_bound_of(trait_), trait_) {
                RefRule::Never
            } else {
                RefRule::Inherits
            };
            let id = lowering.trait_decl(trait_);
            let def = program.traits.entry(id).or_default();
            def.arg_defaults = defaults;
            def.on_ref = on_ref;
        }

        for (&def, &newtype) in tysys.all_newtypes.iter() {
            let ResolvedType::Newtype { base_type, .. } = table.get(newtype) else {
                continue;
            };
            let Some(base) = lowering.type_id(&table, *base_type, &|_, _| None) else {
                continue;
            };
            program.types.insert(
                lowering.type_decl(def),
                TypeDef {
                    newtype_base: Some(base),
                },
            );
        }
        for (&def, info) in tysys.all_generic_newtypes.iter() {
            let param = |name: &str| -> Option<ParamKind> {
                info.type_params
                    .iter()
                    .position(|p| p == name)
                    .map(|i| ParamKind::Type(u32::try_from(i).expect("fewer than 2^32 params")))
            };
            let Some(base) = lowering.ast_type(&info.base_type_ast, &param, resolutions, None)
            else {
                continue;
            };
            program.types.insert(
                lowering.type_decl(def),
                TypeDef {
                    newtype_base: Some(base),
                },
            );
        }

        let (declarations, variants) = Self::declarations(tysys, &table, &mut lowering);
        for item in Self::DERIVED {
            let Some(trait_) = tysys.compiler_trait_def(item) else {
                continue;
            };
            let trait_ = lowering.trait_decl(trait_);
            // A variant derives `Eq` and serde, never `Ord` (spec, Structs:
            // auto-derived traits).
            let eligible: Vec<Declaration> = if item == crate::compiler_item::CompilerItem::Ord {
                declarations.clone()
            } else {
                declarations.iter().chain(&variants).cloned().collect()
            };
            let derived = derive(&program, trait_, &eligible);
            for def in derived.impls {
                let id = program.next_impl_id();
                program.add_impl(id, def);
            }
        }

        // A `flags` type is stored as a `u32` and inherits its impls, the way
        // a newtype inherits its base's; its own impls are asked first.
        let u32_ = SolverType::Decl(lowering.builtin("u32"), vec![]);
        for &def in tysys.all_flags_cases.keys() {
            program.types.insert(
                lowering.type_decl(def),
                TypeDef {
                    newtype_base: Some(u32_.clone()),
                },
            );
        }

        Self::state_reflect_facts(tysys, &table, &mut lowering, &mut program);

        let stated: IndexSet<DefId> = Self::DERIVED
            .into_iter()
            .chain([crate::compiler_item::CompilerItem::Inspect])
            .chain(Self::REFLECT.into_iter().map(|(item, _)| item))
            .chain(Self::OPERATORS)
            .filter_map(|item| tysys.compiler_trait_def(item))
            .collect();
        let excluded = tysys
            .trait_env
            .impl_headers
            .values()
            .filter_map(|h| h.trait_ref)
            .chain(tysys.trait_env.supertrait_closures().map(|(t, _)| *t))
            .filter(|t| tysys.compiler_item_of_trait(*t).is_some() && !stated.contains(t))
            .collect();

        Self {
            program,
            lowering,
            excluded,
        }
    }

    /// The operator items a primitive carries, by `primitive_has_operator`.
    const OPERATORS: [crate::compiler_item::CompilerItem; 12] = [
        crate::compiler_item::CompilerItem::Add,
        crate::compiler_item::CompilerItem::Sub,
        crate::compiler_item::CompilerItem::Mul,
        crate::compiler_item::CompilerItem::Div,
        crate::compiler_item::CompilerItem::Rem,
        crate::compiler_item::CompilerItem::Neg,
        crate::compiler_item::CompilerItem::BitAnd,
        crate::compiler_item::CompilerItem::BitOr,
        crate::compiler_item::CompilerItem::BitXor,
        crate::compiler_item::CompilerItem::BitNot,
        crate::compiler_item::CompilerItem::Shl,
        crate::compiler_item::CompilerItem::Shr,
    ];

    /// The reflection kinds, each with the shape it holds of. The root holds
    /// of every kind and is ungated by field visibility: naming a type is not
    /// enumerating it (WEP 2026-06-13).
    const REFLECT: [(crate::compiler_item::CompilerItem, ReflectKind); 5] = [
        (
            crate::compiler_item::CompilerItem::Reflect,
            ReflectKind::Any,
        ),
        (
            crate::compiler_item::CompilerItem::ReflectStruct,
            ReflectKind::Struct,
        ),
        (
            crate::compiler_item::CompilerItem::ReflectVariant,
            ReflectKind::Variant,
        ),
        (
            crate::compiler_item::CompilerItem::ReflectEnum,
            ReflectKind::Enum,
        ),
        (
            crate::compiler_item::CompilerItem::ReflectFlags,
            ReflectKind::Flags,
        ),
    ];

    /// State each declaration's reflection kinds as facts: a struct is
    /// `ReflectStruct` where every field is visible, a variant, plain enum or
    /// flags its own kind everywhere, and each of them `Reflect`. A sealed
    /// reflection member reflects nothing.
    fn state_reflect_facts(
        tysys: &TypeSystem,
        table: &TypeTable,
        lowering: &mut Lowering,
        program: &mut Program,
    ) {
        let kinds: Vec<(TraitDeclId, ReflectKind)> = Self::REFLECT
            .into_iter()
            .filter_map(|(item, kind)| {
                Some((lowering.trait_decl(tysys.compiler_trait_def(item)?), kind))
            })
            .collect();
        let modules: Vec<(ModuleId, ModuleSource)> = lowering
            .modules
            .iter()
            .map(|(module, &id)| (ModuleId(id), module.clone()))
            .collect();
        let state = |program: &mut Program,
                     lowering: &mut Lowering,
                     def: DefId,
                     kind: ReflectKind,
                     visible_from: Option<Vec<ModuleId>>| {
            let head = lowering.type_decl(def);
            for &(trait_, stated) in &kinds {
                if stated == ReflectKind::Any {
                    program
                        .facts
                        .insert((head, trait_), Fact { visible_from: None });
                } else if stated == kind {
                    program.facts.insert(
                        (head, trait_),
                        Fact {
                            visible_from: visible_from.clone(),
                        },
                    );
                }
            }
        };
        for (&def, info) in tysys.all_struct_fields.iter() {
            // Kinds are disjoint: a variant registers struct-shaped fields for
            // its payload under its own name, and is a variant.
            if tysys.all_variant_cases.contains_key(&def)
                || table.is_sealed_reflect_member(info.defined_at)
            {
                continue;
            }
            // Every field must be reachable from the asking module: one
            // synthesized impl enumerates them all (WEP 2026-06-13).
            let visible_from = if info.fields.is_empty() {
                None
            } else {
                Some(
                    modules
                        .iter()
                        .filter(|(_, module)| {
                            *module == info.module_source || {
                                let same_package = info.module_source.same_package(module);
                                info.fields
                                    .iter()
                                    .all(|(_, _, vis)| vis.reachable_from(same_package))
                            }
                        })
                        .map(|(id, _)| *id)
                        .collect(),
                )
            };
            state(program, lowering, def, ReflectKind::Struct, visible_from);
        }
        for (&def, info) in tysys.all_variant_cases.iter() {
            if !table.is_sealed_reflect_member(info.defined_at) {
                state(program, lowering, def, ReflectKind::Variant, None);
            }
        }
        for (&def, info) in tysys.all_enum_cases.iter() {
            if !table.is_sealed_reflect_member(info.defined_at) {
                state(program, lowering, def, ReflectKind::Enum, None);
            }
        }
        for (&def, info) in tysys.all_flags_cases.iter() {
            if table.is_reflect_eligible(info.type_id) {
                state(program, lowering, def, ReflectKind::Flags, None);
            }
        }
    }

    /// Every declaration as [`derive`] reads it: structs, plain enums and
    /// flags first, variants second, since a variant is eligible for fewer
    /// traits. A declaration with a member the lowering cannot express — a
    /// function-typed field — is left out, which is what makes it not derive.
    fn declarations(
        tysys: &TypeSystem,
        table: &TypeTable,
        lowering: &mut Lowering,
    ) -> (Vec<Declaration>, Vec<Declaration>) {
        let mut out = Vec::new();
        let mut variants = Vec::new();
        let by_index = |_: &str, index: u32| Some(index);
        for (&def, info) in tysys.all_struct_fields.iter() {
            let members: Option<Vec<SolverType>> = info
                .fields
                .iter()
                .map(|(_, ty, _)| lowering.type_id(table, *ty, &by_index))
                .collect();
            let Some(members) = members else { continue };
            out.push(Declaration {
                id: lowering.type_decl(def),
                params: u32::try_from(info.type_param_type_ids.len()).expect("param count"),
                members,
                module: lowering.module(&info.module_source),
            });
        }
        for (&def, info) in tysys.all_variant_cases.iter() {
            let members: Option<Vec<SolverType>> = info
                .cases
                .iter()
                .filter(|c| c.payload != TypeTable::UNIT)
                .map(|c| lowering.type_id(table, c.payload, &by_index))
                .collect();
            let Some(members) = members else { continue };
            variants.push(Declaration {
                id: lowering.type_decl(def),
                params: u32::try_from(info.type_param_type_ids.len()).expect("param count"),
                members,
                module: lowering.module(&info.module_source),
            });
        }
        for (&def, info) in tysys.all_enum_cases.iter() {
            out.push(Declaration {
                id: lowering.type_decl(def),
                params: 0,
                members: vec![],
                module: lowering.module(&info.module_source),
            });
        }
        for (&def, info) in tysys.all_flags_cases.iter() {
            out.push(Declaration {
                id: lowering.type_decl(def),
                params: 0,
                members: vec![],
                module: lowering.module(&info.module_source),
            });
        }
        (out, variants)
    }

    /// The solver's answer to the question `type_implements_trait` just
    /// answered, or `None` where the question is outside what the lowering
    /// states: an excluded trait, a shape it cannot express, a bound it cannot
    /// name.
    pub(super) fn answer(
        &self,
        tysys: &TypeSystem,
        ctx: &super::scope::Scope,
        scope: &super::types::TypeLookup,
        type_id: TypeId,
        trait_: DefId,
    ) -> Option<(bool, String)> {
        if self.excluded.contains(&trait_) {
            return None;
        }
        let trait_id = self.lowering.known_trait(trait_)?;
        let names: Vec<&String> = ctx.trait_ctx.type_param_bounds.keys().collect();
        let mut env = Env::default();
        for bounds in ctx.trait_ctx.type_param_bounds.values() {
            let mut ids = Vec::with_capacity(bounds.len());
            for bound in bounds {
                let def = bound
                    .resolved
                    .or_else(|| tysys.resolutions.declared(bound.id))?;
                ids.push(self.lowering.known_trait(def)?);
            }
            env.param_bounds.push(ids);
        }
        let param = |name: &str, _: u32| -> Option<u32> {
            names
                .iter()
                .position(|n| n.as_str() == name)
                .map(|p| u32::try_from(p).expect("fewer than 2^32 params"))
        };
        let ty = self
            .lowering
            .type_id(&tysys.type_table.borrow(), type_id, &param)?;
        let module = self.lowering.known_module(scope.current_module_source)?;
        let answer = holds(&self.program, &env, &ty, trait_id, module);
        let name_of = |id: u32| -> String {
            self.lowering
                .decls
                .iter()
                .find(|(_, i)| **i == id)
                .map_or_else(
                    || format!("#{id}"),
                    |(key, _)| match key {
                        DeclKey::Def(def) => tysys.resolutions.defs().name(*def).to_string(),
                        DeclKey::Builtin(name) => name.clone(),
                    },
                )
        };
        let env_names: Vec<(&String, Vec<String>)> = names
            .iter()
            .zip(&env.param_bounds)
            .map(|(name, bounds)| (*name, bounds.iter().map(|b| name_of(b.0)).collect()))
            .collect();
        let detail = format!(
            "lowered as {ty:?} : {trait_id:?} under {env_names:?} from {module:?}; answer {answer:?}; impls of the trait: {:?}",
            self.program
                .impls
                .iter()
                .filter(|(_, d)| d.trait_ == Some(trait_id))
                .map(|(id, d)| (id, name_of(head_of(&d.target)), d))
                .collect::<Vec<_>>()
        );
        Some((answer.is_some(), detail))
    }
}

/// The declaration id at a type's head, for naming it in a diagnostic.
fn head_of(ty: &SolverType) -> u32 {
    match ty {
        SolverType::Decl(head, _) => head.0,
        SolverType::Ref { inner, .. } => head_of(inner),
        SolverType::Param(_) | SolverType::Pack(_) | SolverType::Tuple(_) => u32::MAX,
    }
}
