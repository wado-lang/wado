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
    ArgDefault, AssocId, Candidate, Declaration, Env, Fact, ImplDef, ImplId, ImplOrigin, MethodId,
    ModuleId, ModuleScope, ParamDef, Pin, Program, RefRule, Selection, SolverType, TraitDeclId,
    TypeDeclId, TypeDef, candidates, derive, holds, rank,
};

use super::trait_env::{BlanketReceiver, ImplHeader};
use super::trait_query::{OnBoundTrait, primitive_has_operator};
use super::tysys::TypeSystem;

/// What a [`TypeDeclId`] stands for: a declaration, or a shape no module
/// declares — a primitive, which the compiler answers for by name, or an
/// anonymous struct.
#[derive(PartialEq, Eq, Hash)]
enum DeclKey {
    Def(DefId),
    Builtin(String),
    /// One head for every anonymous struct, its field types as the arguments.
    /// A literal mints its shape after the program is built, and no impl can
    /// name one, so what reaches it — a blanket over its `Reflect*` facts —
    /// reads the same of every shape.
    AnonymousStruct,
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
    /// The impl block each lowered impl came from, so a selection the compiler
    /// made and one the solver made name the same thing. A derived impl names
    /// the blanket its body comes from; a primitive's impl, or a body the
    /// compiler supplies with no blanket, is written by no block and is absent.
    impl_defs: IndexMap<ImplId, DefId>,
    /// The `Reflect*`-bounded blanket a derived body comes from, by the trait
    /// and the reflection kind it bounds on. Lookup collects that block for a
    /// derived body, so a `Derived` impl is named to it.
    derivation_source: IndexMap<(TraitDeclId, CompilerItem), DefId>,
    /// Heads the program names but reads no members of: the anonymous head,
    /// and a struct declared in a body, whose fields annotate resolves after
    /// the program is built. `derive` never saw one, so the differential
    /// skips a question mentioning it.
    opaque_heads: IndexSet<TypeDeclId>,
}

/// The spelling a function type's head is keyed by. `fn mut` is a shape of its
/// own, since a closure that may write its captures is not the other.
fn fn_shape_name(is_mut: bool) -> &'static str {
    if is_mut { "fn mut" } else { "fn" }
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

    fn anonymous_struct(&mut self) -> TypeDeclId {
        TypeDeclId(intern(&mut self.decls, DeclKey::AnonymousStruct))
    }

    /// The head every anonymous struct lowers under, interned by `build`.
    fn anonymous_head(&self) -> TypeDeclId {
        self.known_type(&DeclKey::AnonymousStruct)
            .expect("the anonymous head is interned before the program is read")
    }

    /// The head a function type lowers under.
    fn fn_shape(&mut self, is_mut: bool) -> TypeDeclId {
        self.builtin(fn_shape_name(is_mut))
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
            // `impl Greet for geo::Tag` names a declaration like any other; a
            // qualified spelling is never a builtin shape.
            Type::NamespacedGeneric(generic) => {
                let head = self.type_decl(resolutions.declared(generic.id)?);
                let args = generic
                    .args
                    .iter()
                    .map(|arg| self.ast_type(arg, param, resolutions, self_type))
                    .collect::<Option<Vec<_>>>()?;
                Some(SolverType::Decl(head, args))
            }
            // A function type is a shape keyed by its spelling, as a builtin
            // is, and its arguments are its parameters then its return — n + 1
            // of them, so no two arities collide. Selection reads it for
            // equality and for matching, and neither needs more.
            Type::Function(f) => {
                let mut args = f
                    .params
                    .iter()
                    .chain(std::iter::once(&f.return_type))
                    .map(|ty| self.ast_type(ty, param, resolutions, self_type))
                    .collect::<Option<Vec<_>>>()?;
                args.shrink_to_fit();
                Some(SolverType::Decl(self.fn_shape(f.is_mut), args))
            }
            Type::Infer(_) | Type::Error(_) => None,
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
            match mapped_elem {
                None => param(name, *index).map(SolverType::Pack),
                Some(_) => None,
            }
        };
        let instance = |def: DefId, type_args: &[TypeId]| {
            if self.tuple == Some(def) {
                let elems = type_args
                    .iter()
                    .map(|&a| tuple_elem(a))
                    .collect::<Option<Vec<_>>>()?;
                return Some(SolverType::Tuple(elems));
            }
            let args = type_args
                .iter()
                .map(|&a| self.type_id(table, a, param))
                .collect::<Option<Vec<_>>>()?;
            decl(DeclKey::Def(def), args)
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
            ResolvedType::Struct {
                def: crate::tir::StructDef::Decl(def),
                type_args,
            } => instance(*def, type_args),
            // A literal's shape lowers under the one anonymous head, its field
            // types as the arguments. A synthetic shape — a closure
            // environment — declares no fields the compiler reflects, so it
            // stays unsaid.
            ResolvedType::Struct {
                def: crate::tir::StructDef::Anon(shape),
                ..
            } => {
                if table.anon_struct_is_synthetic(*shape) {
                    return None;
                }
                let fields = table
                    .anon_struct_fields(*shape)
                    .iter()
                    .map(|(_, ty)| self.type_id(table, *ty, param))
                    .collect::<Option<Vec<_>>>()?;
                decl(DeclKey::AnonymousStruct, fields)
            }
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
            // Outside a tuple, a pack stands for one of its elements: a rigid
            // type carrying the pack's bounds, at the pack's slot. A mapped
            // one is `R` at that element.
            ResolvedType::TypePack {
                name,
                index,
                mapped_elem: None,
            } => param(name, *index).map(SolverType::Param),
            ResolvedType::TypePack {
                mapped_elem: Some(mapped),
                ..
            } => self.type_id(table, *mapped, param),
            ResolvedType::Function {
                is_mut,
                params,
                return_type,
                ..
            } => {
                let args = params
                    .iter()
                    .chain(std::iter::once(return_type))
                    .map(|&a| self.type_id(table, a, param))
                    .collect::<Option<Vec<_>>>()?;
                decl(DeclKey::Builtin(fn_shape_name(*is_mut).to_string()), args)
            }
            // `impl Inspect for !` is written in the prelude, so the receiver
            // side names the same shape.
            ResolvedType::Never => decl(DeclKey::Builtin("!".to_string()), vec![]),
            // A projection on a rigid parameter, satisfying what its trait
            // declares of the associated type. One built under no trait names
            // nothing the solver can read.
            ResolvedType::AssocTypeProjection {
                param_id,
                assoc_name,
                owning_trait,
                ..
            } => {
                let trait_ = self.known_trait((*owning_trait)?)?;
                Some(SolverType::Projection {
                    base: Box::new(self.type_id(table, *param_id, param)?),
                    trait_,
                    assoc: self.known_assoc(trait_, assoc_name)?,
                })
            }
            ResolvedType::Reactive(_)
            | ResolvedType::InferVar(_)
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
    for (&def, header) in impl_headers {
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
        lowering.impl_defs.insert(id, def);
        if let Some(implemented) = implemented {
            let own = header
                .methods
                .iter()
                .map(|m| lowering.method(&m.name))
                .collect();
            program.impl_methods.insert(id, own);
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

/// A type standing for a lowered head, for what the compiler reads off a type
/// rather than off an impl. `None` for a head that is no type (a trait), or
/// whose type is minted after the program is built (a body-local struct).
fn representative(
    tysys: &TypeSystem,
    table: &TypeTable,
    tuple: Option<DefId>,
    key: &DeclKey,
) -> Option<ResolvedType> {
    use crate::defs::DefKind;
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
            if name == TypeTable::UNIT_TYPE_NAME {
                Some(ResolvedType::Unit)
            } else if name == "!" {
                Some(ResolvedType::Never)
            } else if name == TypeTable::ARRAY_TYPE_NAME {
                Some(ResolvedType::BuiltinArray(TypeTable::UNIT))
            } else if name == fn_shape_name(false) || name == fn_shape_name(true) {
                Some(ResolvedType::Function {
                    is_mut: name == fn_shape_name(true),
                    params: Vec::new(),
                    return_type: TypeTable::UNIT,
                    effects: Vec::new(),
                    stores: Vec::new(),
                })
            } else {
                None
            }
        }
        DeclKey::AnonymousStruct => None,
    }
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
            || matches!(
                item,
                CompilerItem::Inspect
                    | CompilerItem::Display
                    | CompilerItem::Default
                    | CompilerItem::Ref
                    | CompilerItem::RefMut
            )
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
        program.tuple = lowering.tuple.map(|def| lowering.type_decl(def));
        Self::intern_declarations(tysys, &mut lowering);
        let derivation_sources = Self::derivation_sources(tysys);
        for (&def, &kind) in &derivation_sources {
            if let Some(trait_) = tysys
                .trait_env
                .impl_headers
                .get(&def)
                .and_then(|header| header.trait_ref)
            {
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
            &tysys.resolutions,
        );
        Self::state_primitive_impls(tysys, &mut lowering, &mut program);
        Self::state_traits(tysys, &mut lowering, &mut program);
        Self::state_scopes(tysys, &mut lowering, &mut program);
        Self::state_newtype_bases(tysys, &table, &mut lowering, &mut program);
        Self::derive_all(tysys, &table, &mut lowering, &mut program);
        Self::name_derived_impls(tysys, &mut lowering, &program);
        Self::state_reflect_facts(tysys, &table, &lowering, &mut program);
        Self::state_type_facts(tysys, &table, &lowering, &mut program);
        Self { program, lowering }
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
                    .find_map(|bound| reflect.get(&bound.decl_ref?).copied())?;
                Some((blanket.def, kind))
            })
            .collect()
    }

    /// Name each derived impl, and each marker demanding one, to the blanket
    /// lookup collects for its body: the source of its trait at the
    /// declaration's reflection kind. A marker block declares no method, so
    /// lookup never collects it. A trait the compiler derives without a blanket
    /// (`Eq`, `Ord`) stays unnamed.
    fn name_derived_impls(tysys: &TypeSystem, lowering: &mut Lowering, program: &Program) {
        let kind_of = |def: DefId| {
            if tysys.all_struct_fields.contains_key(&def) {
                CompilerItem::ReflectStruct
            } else if tysys.all_variant_cases.contains_key(&def) {
                CompilerItem::ReflectVariant
            } else if tysys.all_enum_cases.contains_key(&def) {
                CompilerItem::ReflectEnum
            } else if tysys.all_flags_cases.contains_key(&def) {
                CompilerItem::ReflectFlags
            } else {
                CompilerItem::ReflectNewtype
            }
        };
        for (&id, def) in &program.impls {
            if !matches!(def.origin, ImplOrigin::Derived | ImplOrigin::Marker) {
                continue;
            }
            let (Some(trait_), SolverType::Decl(head, _)) = (def.trait_, &def.target) else {
                continue;
            };
            let Some((DeclKey::Def(decl), _)) = lowering.decls.get_index(head.0 as usize) else {
                continue;
            };
            if let Some(&source) = lowering.derivation_source.get(&(trait_, kind_of(*decl))) {
                lowering.impl_defs.insert(id, source);
            }
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
        let anonymous = lowering.anonymous_struct();
        lowering.opaque_heads.insert(anonymous);
        // A struct declared in a body has its identity here and its fields
        // only once annotate reaches the body.
        let defs = tysys.resolutions.defs();
        for def in defs.iter().filter(|&def| {
            matches!(defs.kind(def), crate::defs::DefKind::Struct) && defs.is_function_local(def)
        }) {
            let head = lowering.type_decl(def);
            lowering.opaque_heads.insert(head);
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
                let trait_ = lowering.trait_decl(trait_);
                // The prelude writes many of these pairs; one impl per pair,
                // or every call on a primitive would rank `Duplicated`.
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
        // A reference is itself the thing a `Ref` bound asks for, as it is
        // its own `Eq`.
        let holds_of_a_reference: Vec<DefId> =
            [CompilerItem::Eq, CompilerItem::Ref, CompilerItem::RefMut]
                .into_iter()
                .filter_map(|item| tysys.compiler_trait_def(item))
                .collect();
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
            let on_ref = if holds_of_a_reference.contains(&trait_) {
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
            let assoc_bounds = header
                .assoc_types
                .iter()
                .map(|assoc| {
                    let bounds = assoc
                        .bounds
                        .iter()
                        .filter_map(|b| b.resolved.or_else(|| tysys.resolutions.declared(b.id)))
                        .map(|def| lowering.trait_decl(def))
                        .collect();
                    (lowering.assoc(id, &assoc.name), bounds)
                })
                .collect();
            let def = program.traits.entry(id).or_default();
            def.arg_defaults = defaults;
            def.on_ref = on_ref;
            def.methods = methods;
            def.assoc_bounds = assoc_bounds;
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
                .into_iter()
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
            state(
                lowering.declared_type(def),
                OnBoundTrait::ReflectStruct,
                visible_from,
            );
        }
        // A literal's fields are all visible, from every module.
        state(lowering.anonymous_head(), OnBoundTrait::ReflectStruct, None);
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
                state(lowering.declared_type(def), kind, None);
            }
        }
    }

    /// The three remaining things the compiler reads off a type rather than off
    /// an impl: a plain `enum`'s `Display`, and the `Ref` / `RefMut` identities.
    /// Each is a fact stated of a declaration, so it answers for every instance
    /// and from every module.
    fn state_type_facts(
        tysys: &TypeSystem,
        table: &TypeTable,
        lowering: &Lowering,
        program: &mut Program,
    ) {
        let trait_of = |item| {
            tysys
                .compiler_trait_def(item)
                .and_then(|def| lowering.known_trait(def))
        };
        let (display, ref_, ref_mut) = (
            trait_of(CompilerItem::Display),
            trait_of(CompilerItem::Ref),
            trait_of(CompilerItem::RefMut),
        );
        let mut fact = |head: Option<TypeDeclId>, trait_: Option<TraitDeclId>| {
            if let (Some(head), Some(trait_)) = (head, trait_) {
                program
                    .facts
                    .insert((head, trait_), Fact { visible_from: None });
            }
        };
        let declared = |def: DefId| Some(lowering.declared_type(def));

        // A plain `enum` derives `Display` over the bare case name, so the
        // bound holds before `synthesize_traits` emits the body.
        for &def in tysys.all_enum_cases.keys() {
            fact(declared(def), display);
        }

        // A struct every one of whose fields has a default derives `Default`
        // from the defaults alone, so the bound holds with no impl written and
        // with no member's own `Default` asked for. A generic one does not:
        // a default is elaborated against the declaration, not an instance.
        let default = trait_of(CompilerItem::Default);
        for (&def, info) in tysys.all_struct_fields.iter() {
            if info.auto_derives_default() {
                fact(declared(def), default);
            }
        }

        // `Ref` and `RefMut` are what `is_ref_identity` and
        // `is_ref_mut_identity` read off a type. Each head is asked through a
        // type standing for it, so the two paths share the one predicate; a
        // head standing for no type (a trait, a head whose type is minted
        // later) states nothing.
        let is_variant = |def: DefId| tysys.all_variant_cases.contains_key(&def);
        for (key, &id) in &lowering.decls {
            let (is_ref, is_ref_mut) = match key {
                DeclKey::Def(_) | DeclKey::Builtin(_) => {
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
                DeclKey::AnonymousStruct => (true, true),
            };
            if is_ref {
                fact(Some(TypeDeclId(id)), ref_);
            }
            if is_ref_mut {
                fact(Some(TypeDeclId(id)), ref_mut);
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
    /// The bounds in force where a question is asked, and the parameter names
    /// they are indexed by. A generic body's `T: Tr` holds because its own
    /// signature says so, not because any impl exists, so no query about `T`
    /// can be answered from the program alone. `None` where a bound names a
    /// trait the lowering never interned, which the caller reads as "outside
    /// what the lowering states".
    fn env_at(&self, tysys: &TypeSystem, ctx: &super::scope::Scope) -> Option<(Env, Vec<String>)> {
        // Every parameter in scope takes a position, bounded or not: an
        // unbounded `T` still appears in a receiver such as `Array<T>`, and a
        // receiver the environment cannot place lowers to nothing.
        let mut env = Env::default();
        for name in ctx.trait_ctx.type_params.keys() {
            let mut ids = Vec::new();
            for bound in ctx
                .trait_ctx
                .type_param_bounds
                .get(name)
                .into_iter()
                .flatten()
            {
                let def = bound
                    .resolved
                    .or_else(|| tysys.resolutions.declared(bound.id))?;
                ids.push(self.lowering.known_trait(def)?);
            }
            env.param_bounds.push(ids);
        }
        Some((env, ctx.trait_ctx.type_params.keys().cloned().collect()))
    }

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
        let (env, names) = self.env_at(tysys, ctx)?;
        let ty =
            self.lowering
                .type_id(&tysys.type_table.borrow(), type_id, &param_index(&names))?;
        // A head the program names without members is one `derive` never saw,
        // so only the compiler answers for it.
        if ty.mentions_decl(&|h| self.lowering.opaque_heads.contains(&h)) {
            return None;
        }
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
        ctx: &super::scope::Scope,
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
        let (env, names) = self.env_at(tysys, ctx)?;
        let ty =
            self.lowering
                .type_id(&tysys.type_table.borrow(), type_id, &param_index(&names))?;
        let ty = match through_ref {
            Some(is_mut) => SolverType::Ref {
                is_mut,
                inner: Box::new(ty),
            },
            None => ty,
        };
        let module = self.lowering.known_module(module)?;
        let mut found = candidates(&self.program, &env, &ty, method, module);
        if let Some(required) = required {
            found.in_scope.retain(|c| c.trait_ == required);
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

    /// The impl block a candidate names: the one it was lowered from, or for a
    /// derived body the `Reflect*` blanket lookup collects for it. `None` for a
    /// body the compiler supplies with no block at all, which is how a
    /// `TraitMethodMatch` says it too.
    fn impl_def_of(&self, impl_: ImplId) -> Option<DefId> {
        self.lowering.impl_defs.get(&impl_).copied()
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
                        DeclKey::AnonymousStruct => "{..}".to_string(),
                    },
                )
        };
        // Positions are `env_at`'s: every parameter in scope, in order.
        let env: Vec<(&String, Vec<String>)> = ctx
            .trait_ctx
            .type_params
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
                    | SolverType::Tuple(_)
                    | SolverType::Projection { .. } => String::new(),
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
    /// Several impls of one pair, which coherence rejects where they are
    /// written.
    Duplicated(Vec<Option<DefId>>),
}

/// Where each type parameter sits in the environment [`SolverBridge::env_at`]
/// built, which is what gives a rigid parameter its [`SolverType::Param`].
fn param_index(names: &[String]) -> impl Fn(&str, u32) -> Option<u32> + '_ {
    move |name: &str, _: u32| {
        names
            .iter()
            .position(|n| n == name)
            .map(|p| u32::try_from(p).expect("fewer than 2^32 params"))
    }
}

/// A `type_implements_trait` question as the solver reads it.
struct Question {
    env: Env,
    ty: SolverType,
    trait_: TraitDeclId,
    module: ModuleId,
}
