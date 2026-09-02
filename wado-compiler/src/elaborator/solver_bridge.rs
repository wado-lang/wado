//! The lowering from the compiler's tables into the solver's [`Program`], and
//! the differential checking the solver's answers against the compiler's own.

use crate::ast::Type;
use crate::compiler_item::CompilerItem;
use crate::defs::DefId;
use crate::hashmap::{IndexMap, IndexSet};
use crate::module_source::ModuleSource;
use crate::name::is_builtin_shape_name;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};
use crate::trait_solver::{
    ArgDefault, AssocId, Declaration, Env, Fact, ImplDef, ImplOrigin, ModuleId, ParamDef, Pin,
    Program, RefRule, SolverType, TraitDeclId, TypeDeclId, TypeDef, derive, holds,
};

use super::trait_env::ImplHeader;
use super::trait_query::primitive_has_operator;
use super::tysys::TypeSystem;

/// What a [`TypeDeclId`] stands for: a declaration, or a shape no module
/// declares — a primitive, which the compiler answers for by name.
#[derive(PartialEq, Eq, Hash)]
enum DeclKey {
    Def(DefId),
    Builtin(String),
}

/// How an impl's parameter is spelled where a type mentions it.
#[derive(Clone, Copy)]
enum ParamKind {
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

    fn trait_decl(&mut self, def: DefId) -> TraitDeclId {
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

    /// The id a declaration was given, if it was ever lowered.
    fn known_trait(&self, def: DefId) -> Option<TraitDeclId> {
        self.decls.get(&DeclKey::Def(def)).map(|&i| TraitDeclId(i))
    }

    fn known_type(&self, key: &DeclKey) -> Option<TypeDeclId> {
        self.decls.get(key).map(|&i| TypeDeclId(i))
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

    /// One AST type as the solver reads it, or `None` for a shape it has no way
    /// to say. `param` names the surrounding item's own parameters and
    /// `self_type` what `Self` means here.
    fn ast_type(
        &mut self,
        ty: &Type,
        param: &dyn Fn(&str) -> Option<ParamKind>,
        resolutions: &crate::resolve::Resolutions,
        self_type: Option<&SolverType>,
    ) -> Option<SolverType> {
        // A builtin shape is keyed by its spelling, as `ImplTargetKey::of_decl`
        // keys it.
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

    /// One resolved type as the solver reads it, or `None` for a shape it has
    /// no way to say. `param` gives a rigid type parameter its position.
    fn type_id(
        &self,
        table: &TypeTable,
        id: TypeId,
        param: &dyn Fn(&str, u32) -> Option<u32>,
    ) -> Option<SolverType> {
        let decl = |key: DeclKey, args: Vec<SolverType>| {
            self.known_type(&key).map(|id| SolverType::Decl(id, args))
        };
        let instance = |def: DefId, type_args: &[TypeId]| {
            let args = type_args
                .iter()
                .map(|&a| self.type_id(table, a, param))
                .collect::<Option<Vec<_>>>()?;
            if self.tuple == Some(def) {
                Some(SolverType::Tuple(args))
            } else {
                decl(DeclKey::Def(def), args)
            }
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
            ResolvedType::Struct { def, type_args } => instance(def.decl()?, type_args),
            ResolvedType::Enum { def }
            | ResolvedType::Resource { def }
            | ResolvedType::Variant { def }
            | ResolvedType::Flags { def } => instance(*def, &[]),
            ResolvedType::GenericResource { def, type_args }
            | ResolvedType::GenericInstance { def, type_args }
            | ResolvedType::Newtype { def, type_args, .. } => instance(*def, type_args),
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
/// back the header each [`ImplId`](crate::trait_solver::ImplId) stands for so
/// a finding can be given a span. A header the lowering cannot express is
/// dropped, never approximated.
pub(super) fn lower_impls(
    lowering: &mut Lowering,
    program: &mut Program,
    impl_headers: &IndexMap<DefId, ImplHeader>,
    resolutions: &crate::resolve::Resolutions,
) -> Vec<DefId> {
    let mut sources: Vec<DefId> = Vec::new();
    for (&impl_def, header) in impl_headers {
        // `impl FromIterator for List<T>` binds `T` without an `impl<T>`: a
        // target argument no declaration answers is an implicit parameter,
        // positioned after the declared ones.
        let mut implicit: Vec<&str> = Vec::new();
        if let Type::Generic(generic) = &header.ty {
            for arg in &generic.args {
                if let Type::Named(named) = arg
                    && !header.type_params.iter().any(|p| p.name == named.name)
                    && resolutions.declared(named.id).is_none()
                    && !is_builtin_shape_name(&named.name)
                {
                    implicit.push(named.name.as_str());
                }
            }
        }
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
        let mut trait_args = Vec::new();
        if let Some(Type::Generic(generic)) = header.trait_type.as_ref() {
            let lowered: Option<Vec<SolverType>> = generic
                .args
                .iter()
                .map(|arg| lowering.ast_type(arg, &param, resolutions, Some(&target)))
                .collect();
            let Some(lowered) = lowered else {
                continue;
            };
            trait_args = lowered;
        }
        let implemented = header.trait_ref.map(|t| lowering.trait_decl(t));
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
        if let Some(implemented) = implemented {
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
        sources.push(impl_def);
    }
    sources
}

/// The shape a reflection kind holds of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReflectKind {
    Any,
    Struct,
    Variant,
    Enum,
    Flags,
}

/// The solver's view of the whole program, and the differential that checks
/// its answers against the path in use.
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

    /// The reflection kinds, each with the shape it holds of. The root holds
    /// of every kind, ungated by field visibility (WEP 2026-06-13).
    const REFLECT: [(CompilerItem, ReflectKind); 5] = [
        (CompilerItem::Reflect, ReflectKind::Any),
        (CompilerItem::ReflectStruct, ReflectKind::Struct),
        (CompilerItem::ReflectVariant, ReflectKind::Variant),
        (CompilerItem::ReflectEnum, ReflectKind::Enum),
        (CompilerItem::ReflectFlags, ReflectKind::Flags),
    ];

    pub(crate) fn build(tysys: &TypeSystem) -> Self {
        let mut lowering = Lowering::default();
        let mut program = Program::new();
        let table = tysys.type_table.borrow();
        lowering.tuple = table.compiler_item_def(CompilerItem::Tuple);
        Self::intern_declarations(tysys, &mut lowering);
        lower_impls(
            &mut lowering,
            &mut program,
            &tysys.trait_env.impl_headers,
            &tysys.resolutions,
        );
        Self::state_primitive_impls(tysys, &mut lowering, &mut program);
        Self::state_traits(tysys, &mut lowering, &mut program);
        Self::state_newtype_bases(tysys, &table, &mut lowering, &mut program);
        Self::derive_all(tysys, &table, &mut lowering, &mut program);
        Self::state_reflect_facts(tysys, &table, &lowering, &mut program);
        Self {
            program,
            lowering,
            excluded: Self::excluded(tysys),
        }
    }

    /// Intern every declaration and module up front, so a query lowers without
    /// interning and a shape nothing lowered is unknown to it.
    fn intern_declarations(tysys: &TypeSystem, lowering: &mut Lowering) {
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
        let defs = tysys.resolutions.defs();
        for module in tysys
            .module_visible_types
            .keys()
            .chain(defs.iter().map(|def| defs.module(def)))
        {
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
                program.push_impl(ImplDef {
                    trait_: Some(lowering.trait_decl(trait_)),
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
        for (trait_, closure) in tysys.trait_env.supertrait_closures() {
            let id = lowering.trait_decl(*trait_);
            program.traits.entry(id).or_default().supertraits = closure
                .iter()
                .map(|b| lowering.trait_decl(b.decl))
                .collect();
        }
        if let Some(inspect) = tysys.compiler_trait_def(CompilerItem::Inspect) {
            let id = lowering.trait_decl(inspect);
            program.traits.entry(id).or_default().holds_for_all = true;
        }
        let eq = tysys.compiler_trait_def(CompilerItem::Eq);
        for (&trait_, header) in &tysys.trait_env.trait_decl_headers {
            let defaults: Vec<Option<ArgDefault>> = header
                .type_params
                .iter()
                .map(|p| {
                    p.default.as_ref().map(|default| match default {
                        Type::Named(named) if named.name == "Self" => ArgDefault::SelfType,
                        other => lowering
                            .ast_type(other, &|_| None, &tysys.resolutions, None)
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
    }

    /// A newtype inherits its base's impls. A `flags` type is stored as a
    /// `u32` and inherits the same way.
    fn state_newtype_bases(
        tysys: &TypeSystem,
        table: &TypeTable,
        lowering: &mut Lowering,
        program: &mut Program,
    ) {
        let u32_ = SolverType::Decl(lowering.builtin("u32"), vec![]);
        let mut newtype_base = |head: TypeDeclId, base: SolverType| {
            program.types.insert(
                head,
                TypeDef {
                    newtype_base: Some(base),
                },
            );
        };
        for (&def, &newtype) in tysys.all_newtypes.iter() {
            let ResolvedType::Newtype { base_type, .. } = table.get(newtype) else {
                continue;
            };
            if let Some(base) = lowering.type_id(table, *base_type, &|_, _| None) {
                newtype_base(lowering.declared_type(def), base);
            }
        }
        for (&def, info) in tysys.all_generic_newtypes.iter() {
            let param = |name: &str| -> Option<ParamKind> {
                info.type_params
                    .iter()
                    .position(|p| p == name)
                    .map(|i| ParamKind::Type(u32::try_from(i).expect("fewer than 2^32 params")))
            };
            if let Some(base) =
                lowering.ast_type(&info.base_type_ast, &param, &tysys.resolutions, None)
            {
                newtype_base(lowering.declared_type(def), base);
            }
        }
        for &def in tysys.all_flags_cases.keys() {
            newtype_base(lowering.declared_type(def), u32_.clone());
        }
    }

    /// The impls the declarations derive. A variant derives `Eq` and serde,
    /// never `Ord` (spec, Structs: auto-derived traits), so the variants come
    /// last and `Ord` stops before them.
    fn derive_all(
        tysys: &TypeSystem,
        table: &TypeTable,
        lowering: &mut Lowering,
        program: &mut Program,
    ) {
        let (declarations, variants) = Self::declarations(tysys, table, lowering);
        let variants_from = declarations.len();
        let declarations: Vec<Declaration> = [declarations, variants].concat();
        for item in Self::DERIVED {
            let Some(trait_) = tysys.compiler_trait_def(item) else {
                continue;
            };
            let eligible = if item == CompilerItem::Ord {
                &declarations[..variants_from]
            } else {
                &declarations[..]
            };
            for def in derive(program, lowering.trait_decl(trait_), eligible).impls {
                program.push_impl(def);
            }
        }
    }

    /// State each declaration's reflection kinds as facts. A struct's kind
    /// holds only from the modules that see every field; a sealed reflection
    /// member reflects nothing.
    fn state_reflect_facts(
        tysys: &TypeSystem,
        table: &TypeTable,
        lowering: &Lowering,
        program: &mut Program,
    ) {
        let kinds: Vec<(TraitDeclId, ReflectKind)> = Self::REFLECT
            .into_iter()
            .filter_map(|(item, kind)| {
                Some((lowering.known_trait(tysys.compiler_trait_def(item)?)?, kind))
            })
            .collect();
        let eligible = |decl: crate::ast::AstId| !table.is_sealed_reflect_member(decl);
        let mut state = |def: DefId, kind: ReflectKind, visible_from: Option<Vec<ModuleId>>| {
            let head = lowering.declared_type(def);
            for &(trait_, stated) in &kinds {
                let visible_from = match stated {
                    ReflectKind::Any => None,
                    stated if stated == kind => visible_from.clone(),
                    _ => continue,
                };
                program.facts.insert((head, trait_), Fact { visible_from });
            }
        };
        for (&def, info) in tysys.all_struct_fields.iter() {
            // Kinds are disjoint: a variant registers struct-shaped fields for
            // its payload under its own name, and is a variant.
            if tysys.all_variant_cases.contains_key(&def) || !eligible(info.defined_at) {
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
            state(def, ReflectKind::Struct, visible_from);
        }
        for (&def, info) in tysys.all_variant_cases.iter() {
            if eligible(info.defined_at) {
                state(def, ReflectKind::Variant, None);
            }
        }
        for (&def, info) in tysys.all_enum_cases.iter() {
            if eligible(info.defined_at) {
                state(def, ReflectKind::Enum, None);
            }
        }
        for (&def, info) in tysys.all_flags_cases.iter() {
            if table.decl_of_type(info.type_id).is_some_and(eligible) {
                state(def, ReflectKind::Flags, None);
            }
        }
    }

    /// Every declaration as [`derive`] reads it: structs, plain enums and
    /// flags, then the variants. A declaration with a member the lowering
    /// cannot express — a function-typed field — is left out, so it does not
    /// derive.
    fn declarations(
        tysys: &TypeSystem,
        table: &TypeTable,
        lowering: &Lowering,
    ) -> (Vec<Declaration>, Vec<Declaration>) {
        let by_index = |_: &str, index: u32| Some(index);
        let declaration = |def: DefId, params: usize, members, module| Declaration {
            id: lowering.declared_type(def),
            params: u32::try_from(params).expect("fewer than 2^32 params"),
            members,
            module: lowering.declared_module(module),
        };
        let mut out = Vec::new();
        for (&def, info) in tysys.all_struct_fields.iter() {
            let members: Option<Vec<SolverType>> = info
                .fields
                .iter()
                .map(|(_, ty, _)| lowering.type_id(table, *ty, &by_index))
                .collect();
            if let Some(members) = members {
                out.push(declaration(
                    def,
                    info.type_param_type_ids.len(),
                    members,
                    &info.module_source,
                ));
            }
        }
        let memberless = tysys
            .all_enum_cases
            .iter()
            .map(|(&def, info)| (def, &info.module_source))
            .chain(
                tysys
                    .all_flags_cases
                    .iter()
                    .map(|(&def, info)| (def, &info.module_source)),
            );
        for (def, module) in memberless {
            out.push(declaration(def, 0, vec![], module));
        }
        let mut variants = Vec::new();
        for (&def, info) in tysys.all_variant_cases.iter() {
            let members: Option<Vec<SolverType>> = info
                .cases
                .iter()
                .filter(|c| c.payload != TypeTable::UNIT)
                .map(|c| lowering.type_id(table, c.payload, &by_index))
                .collect();
            if let Some(members) = members {
                variants.push(declaration(
                    def,
                    info.type_param_type_ids.len(),
                    members,
                    &info.module_source,
                ));
            }
        }
        (out, variants)
    }

    /// The traits the differential skips: the compiler items whose answers
    /// the lowering does not state.
    fn excluded(tysys: &TypeSystem) -> IndexSet<DefId> {
        let stated: IndexSet<DefId> = Self::DERIVED
            .into_iter()
            .chain([CompilerItem::Inspect])
            .chain(Self::REFLECT.into_iter().map(|(item, _)| item))
            .chain(Self::OPERATORS)
            .filter_map(|item| tysys.compiler_trait_def(item))
            .collect();
        tysys
            .trait_env
            .impl_headers
            .values()
            .filter_map(|h| h.trait_ref)
            .chain(tysys.trait_env.supertrait_closures().map(|(t, _)| *t))
            .filter(|t| tysys.compiler_item_of_trait(*t).is_some() && !stated.contains(t))
            .collect()
    }

    /// The question `type_implements_trait` answered, as the solver reads it,
    /// or `None` where it is outside what the lowering states: an excluded
    /// trait, a shape it cannot express, a bound it cannot name.
    fn question(
        &self,
        tysys: &TypeSystem,
        ctx: &super::scope::Scope,
        scope: &super::types::TypeLookup,
        type_id: TypeId,
        trait_: DefId,
    ) -> Option<Question> {
        if self.excluded.contains(&trait_) {
            return None;
        }
        let trait_ = self.lowering.known_trait(trait_)?;
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
        Some(Question {
            env,
            ty,
            trait_,
            module,
            names: names.into_iter().cloned().collect(),
        })
    }

    /// The solver's answer to the question `type_implements_trait` just
    /// answered, or `None` where the question is outside what the lowering
    /// states.
    pub(super) fn answer(
        &self,
        tysys: &TypeSystem,
        ctx: &super::scope::Scope,
        scope: &super::types::TypeLookup,
        type_id: TypeId,
        trait_: DefId,
    ) -> Option<bool> {
        let q = self.question(tysys, ctx, scope, type_id, trait_)?;
        Some(holds(&self.program, &q.env, &q.ty, q.trait_, q.module).is_some())
    }

    /// What the solver was asked and what it had to answer from, for the
    /// differential's failure message.
    pub(super) fn explain(
        &self,
        tysys: &TypeSystem,
        ctx: &super::scope::Scope,
        scope: &super::types::TypeLookup,
        type_id: TypeId,
        trait_: DefId,
    ) -> String {
        let Some(q) = self.question(tysys, ctx, scope, type_id, trait_) else {
            return "outside what the lowering states".to_string();
        };
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
        let env: Vec<(&String, Vec<String>)> = q
            .names
            .iter()
            .zip(&q.env.param_bounds)
            .map(|(name, bounds)| (name, bounds.iter().map(|b| name_of(b.0)).collect()))
            .collect();
        let answer = holds(&self.program, &q.env, &q.ty, q.trait_, q.module);
        let impls: Vec<_> = self
            .program
            .impls
            .iter()
            .filter(|(_, d)| d.trait_ == Some(q.trait_))
            .map(|(id, d)| {
                let head = match &d.target {
                    SolverType::Decl(head, _) => name_of(head.0),
                    SolverType::Param(_)
                    | SolverType::Pack(_)
                    | SolverType::Ref { .. }
                    | SolverType::Tuple(_) => String::new(),
                };
                (id, head, d)
            })
            .collect();
        format!(
            "lowered as {:?} : {} under {env:?} from {:?}; answer {answer:?}; impls of the trait: {impls:?}",
            q.ty,
            name_of(q.trait_.0),
            q.module,
        )
    }
}

/// A `type_implements_trait` question as the solver reads it.
struct Question {
    env: Env,
    ty: SolverType,
    trait_: TraitDeclId,
    module: ModuleId,
    /// The bound parameters' names, by the position `env` gives them.
    names: Vec<String>,
}
