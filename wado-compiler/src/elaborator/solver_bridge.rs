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
    ArgDefault, AssocId, Declaration, Env, Fact, ImplDef, ImplOrigin, MethodId, ModuleId,
    ModuleScope, ParamDef, Pin, Program, RefRule, SolverType, TraitDeclId, TypeDeclId, TypeDef,
    derive, holds,
};

use super::trait_env::{BlanketReceiver, ImplHeader};
use super::trait_query::{OnBoundTrait, primitive_has_operator};
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
    /// Method names, interned across traits: two traits declaring `describe`
    /// share one id, which is what makes their collision one question.
    methods: IndexMap<String, u32>,
}

/// The id `key` has in `map`, minted at the next index when it has none.
fn intern<K: std::hash::Hash + Eq>(map: &mut IndexMap<K, u32>, key: K) -> u32 {
    let next = u32::try_from(map.len()).expect("a program declares fewer than 2^32 items");
    *map.entry(key).or_insert(next)
}

impl Lowering {
    fn type_decl(&mut self, def: DefId) -> TypeDeclId {
        TypeDeclId(intern(&mut self.decls, DeclKey::Def(def)))
    }

    fn builtin(&mut self, name: &str) -> TypeDeclId {
        TypeDeclId(intern(&mut self.decls, DeclKey::Builtin(name.to_string())))
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
                let args = generic
                    .args
                    .iter()
                    .map(|arg| self.ast_type(arg, param, resolutions, self_type))
                    .collect::<Option<Vec<_>>>()?;
                Some(SolverType::Decl(head, args))
            }
            Type::Tuple(elems) if elems.is_empty() => Some(SolverType::Decl(
                self.builtin(TypeTable::UNIT_TYPE_NAME),
                Vec::new(),
            )),
            Type::Tuple(elems) => elems
                .iter()
                .map(|elem| self.ast_type(elem, param, resolutions, self_type))
                .collect::<Option<Vec<_>>>()
                .map(SolverType::Tuple),
            Type::TypePackSpread(name, _) => match param(name)? {
                ParamKind::Pack(index) | ParamKind::Type(index) => Some(SolverType::Pack(index)),
            },
            Type::Reference(inner) | Type::MutReference(inner) => Some(SolverType::Ref {
                is_mut: matches!(ty, Type::MutReference(_)),
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
            // `()` is the unit declaration, not the empty tuple
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
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => Some(SolverType::Ref {
                is_mut: matches!(table.get(id), ResolvedType::MutRef(_)),
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

/// Lower the impl headers into the program, and hand back the header each
/// [`ImplId`](crate::trait_solver::ImplId) stands for. A header the lowering
/// cannot express is dropped, never approximated.
pub(super) fn lower_impls<'a>(
    lowering: &mut Lowering,
    program: &mut Program,
    impl_headers: impl IntoIterator<Item = (&'a DefId, &'a ImplHeader)>,
    resolutions: &crate::resolve::Resolutions,
) -> Vec<&'a ImplHeader> {
    let mut sources: Vec<&ImplHeader> = Vec::new();
    for (_, header) in impl_headers {
        let implicit = header.implicit_params(resolutions);
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
                    // A pin the lowering cannot spell is dropped, as the
                    // compiler's own check drops one to anything but the
                    // receiver.
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
                    .entry(id)
                    .or_default()
                    .push((lowering.assoc(implemented, &binding.name), ty));
            }
        }
        sources.push(header);
    }
    sources
}

/// The newtype declarations with their base types. A `flags` type sits in the
/// same table and is not one.
fn newtype_decls<'a>(
    tysys: &'a TypeSystem,
    table: &'a TypeTable,
) -> impl Iterator<Item = (DefId, TypeId)> + 'a {
    tysys
        .all_newtypes
        .iter()
        .filter_map(|(&def, &id)| match table.get(id) {
            ResolvedType::Newtype { base_type, .. } => Some((def, *base_type)),
            _ => None,
        })
}

/// The solver's view of the whole program, and the differential that checks
/// its answers against the path in use.
pub(crate) struct SolverBridge {
    program: Program,
    lowering: Lowering,
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
    const REFLECT: [OnBoundTrait; 6] = [
        OnBoundTrait::Reflect,
        OnBoundTrait::ReflectStruct,
        OnBoundTrait::ReflectVariant,
        OnBoundTrait::ReflectEnum,
        OnBoundTrait::ReflectFlags,
        OnBoundTrait::ReflectNewtype,
    ];

    /// Whether the differential asks about `item`: the lowering states the
    /// structural traits, `Inspect`, the reflection kinds and the operators.
    fn states(item: CompilerItem) -> bool {
        Self::DERIVED.contains(&item)
            || item == CompilerItem::Inspect
            || Self::REFLECT
                .iter()
                .any(|kind| kind.compiler_item() == item)
            || Self::OPERATORS.contains(&item)
    }

    pub(crate) fn build(tysys: &TypeSystem) -> Self {
        let mut lowering = Lowering::default();
        let mut program = Program::default();
        let table = tysys.type_table.borrow();
        lowering.tuple = table.compiler_item_def(CompilerItem::Tuple);
        Self::intern_declarations(tysys, &mut lowering);
        let derivation_sources = Self::derivation_sources(tysys);
        lower_impls(
            &mut lowering,
            &mut program,
            tysys
                .trait_env
                .impl_headers
                .iter()
                .filter(|(def, _)| !derivation_sources.contains(*def)),
            &tysys.resolutions,
        );
        Self::state_primitive_impls(tysys, &mut lowering, &mut program);
        Self::state_traits(tysys, &mut lowering, &mut program);
        Self::state_scopes(tysys, &mut lowering, &mut program);
        Self::state_newtype_bases(tysys, &table, &mut lowering, &mut program);
        Self::derive_all(tysys, &table, &mut lowering, &mut program);
        Self::state_reflect_facts(tysys, &table, &lowering, &mut program);
        Self { program, lowering }
    }

    /// The `Reflect*`-bounded value blankets of the structural traits: each is
    /// the derived body's source, which [`derive`] answers for per declaration.
    fn derivation_sources(tysys: &TypeSystem) -> IndexSet<DefId> {
        let reflect: IndexSet<DefId> = Self::REFLECT
            .into_iter()
            .filter_map(|kind| tysys.compiler_trait_def(kind.compiler_item()))
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
            .filter(|blanket| {
                blanket.receiver == BlanketReceiver::Value
                    && blanket
                        .bounds
                        .iter()
                        .any(|bound| bound.decl_ref.is_some_and(|decl| reflect.contains(&decl)))
            })
            .map(|blanket| blanket.def)
            .collect()
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
        for module in tysys.module_visible_types.keys() {
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
            let on_ref = if Some(trait_) == eq {
                RefRule::Always
            } else if tysys.ref_denies_bound(tysys.on_bound_of(trait_), trait_) {
                RefRule::Never
            } else {
                RefRule::Inherits
            };
            let methods = header
                .methods
                .iter()
                .map(|m| lowering.method(&m.name))
                .collect();
            let id = lowering.trait_decl(trait_);
            let def = program.traits.entry(id).or_default();
            def.arg_defaults = defaults;
            def.on_ref = on_ref;
            def.methods = methods;
        }
    }

    /// What each module may name. A trait's methods are candidates at a call
    /// site only where that trait's declaration is in scope there
    /// (WEP 2026-09-01); where its impls were written does not enter.
    fn state_scopes(tysys: &TypeSystem, lowering: &mut Lowering, program: &mut Program) {
        for module in tysys.module_visible_types.keys() {
            // A declaration reachable under two names is in scope once.
            let traits_in_scope: IndexSet<TraitDeclId> = tysys
                .resolutions
                .decls_in_scope(module)
                .filter(|def| tysys.trait_env.decl_index.contains(def))
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
        for (def, base_type) in newtype_decls(tysys, table) {
            if let Some(base) = lowering.type_id(table, base_type, &|_, _| None) {
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

    /// The impls the declarations derive. A variant never derives `Ord`, so
    /// the variants come last and `Ord` stops before them.
    fn derive_all(
        tysys: &TypeSystem,
        table: &TypeTable,
        lowering: &mut Lowering,
        program: &mut Program,
    ) {
        let (mut declarations, variants) = Self::declarations(tysys, table, lowering);
        let variants_from = declarations.len();
        declarations.extend(variants);
        for item in Self::DERIVED {
            let Some(trait_) = tysys.compiler_trait_def(item) else {
                continue;
            };
            let eligible = if item == CompilerItem::Ord {
                &declarations[..variants_from]
            } else {
                &declarations[..]
            };
            derive(program, lowering.trait_decl(trait_), eligible);
        }
    }

    /// State each declaration's reflection kinds as facts. A struct's kind
    /// holds only from the modules that see every field.
    fn state_reflect_facts(
        tysys: &TypeSystem,
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
        let mut state = |def: DefId, kind: OnBoundTrait, visible_from: Option<Vec<ModuleId>>| {
            let head = lowering.declared_type(def);
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
        for (&def, info) in tysys.all_struct_fields.iter() {
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
            state(def, OnBoundTrait::ReflectStruct, visible_from);
        }
        let of = |kind| move |def: &DefId| (*def, kind);
        let memberless = tysys
            .all_variant_cases
            .keys()
            .map(of(OnBoundTrait::ReflectVariant))
            .chain(
                tysys
                    .all_enum_cases
                    .keys()
                    .map(of(OnBoundTrait::ReflectEnum)),
            )
            .chain(
                tysys
                    .all_flags_cases
                    .keys()
                    .map(of(OnBoundTrait::ReflectFlags)),
            )
            .chain(newtype_decls(tysys, table).map(|(def, _)| (def, OnBoundTrait::ReflectNewtype)))
            .chain(
                tysys
                    .all_generic_newtypes
                    .keys()
                    .map(of(OnBoundTrait::ReflectNewtype)),
            );
        for (def, kind) in memberless {
            if eligible(def) {
                state(def, kind, None);
            }
        }
    }

    /// Every declaration as [`derive`] reads it: structs, plain enums and
    /// flags, then the variants. One with a member the lowering cannot express
    /// is left out.
    fn declarations(
        tysys: &TypeSystem,
        table: &TypeTable,
        lowering: &Lowering,
    ) -> (Vec<Declaration>, Vec<Declaration>) {
        let by_index = |_: &str, index: u32| Some(index);
        let lowered = |def: DefId,
                       params: usize,
                       members: &mut dyn Iterator<Item = TypeId>,
                       module: &ModuleSource|
         -> Option<Declaration> {
            let members = members
                .map(|ty| lowering.type_id(table, ty, &by_index))
                .collect::<Option<Vec<_>>>()?;
            Some(Declaration {
                id: lowering.declared_type(def),
                params: u32::try_from(params).expect("fewer than 2^32 params"),
                members,
                module: lowering.declared_module(module),
            })
        };
        let mut out = Vec::new();
        for (&def, info) in tysys.all_struct_fields.iter() {
            out.extend(lowered(
                def,
                info.type_param_type_ids.len(),
                &mut info.fields.iter().map(|(_, ty, _)| *ty),
                &info.module_source,
            ));
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
            out.extend(lowered(def, 0, &mut std::iter::empty(), module));
        }
        let mut variants = Vec::new();
        for (&def, info) in tysys.all_variant_cases.iter() {
            variants.extend(lowered(
                def,
                info.type_param_type_ids.len(),
                &mut info
                    .cases
                    .iter()
                    .filter(|c| c.payload != TypeTable::UNIT)
                    .map(|c| c.payload),
                &info.module_source,
            ));
        }
        (out, variants)
    }

    /// The question `type_implements_trait` answered, as the solver reads it;
    /// `None` where the lowering states nothing about it.
    fn question(
        &self,
        tysys: &TypeSystem,
        ctx: &super::scope::Scope,
        scope: &super::types::TypeLookup,
        type_id: TypeId,
        trait_: DefId,
    ) -> Option<Question> {
        if tysys
            .compiler_item_of_trait(trait_)
            .is_some_and(|item| !Self::states(item))
        {
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
        })
    }

    /// The solver's answer to the question `type_implements_trait` just
    /// answered; `None` where the lowering states nothing about it.
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
        let env: Vec<(&String, Vec<String>)> = ctx
            .trait_ctx
            .type_param_bounds
            .keys()
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
}
