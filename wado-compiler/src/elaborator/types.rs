//! Type definitions used across the elaborator phase.

use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};

use crate::hashmap::{IndexMap, IndexSet};

use crate::analyze::symbol_not_visible_message;
use crate::ast::{self, AstId, Expr, Visibility};
use crate::compiler_host::{Code, Diagnostic};
use crate::defs::DefId;
use crate::elaborator::assert::AssertCaptureContext;
use crate::elaborator::call::DefaultTypeBinding;
use crate::elaborator::reify::ReifyAssertCaptureContext;
use crate::elaborator::sem::decls::ModuleDecls;
use crate::elaborator::sem::imports::canonical_ns_ref;
use crate::elaborator::trait_env::TraitEnv;
use crate::elaborator::trait_query::BoundUnmet;
use crate::elaborator::tysys::TypeSystem;
use crate::hashmap;
use crate::module_source::ModuleSource;
use crate::name::{FqTraitName, FqTypeName, unalias_namespace_member};
use crate::resolve::{Resolution, Resolutions};
use crate::tir::{ResolvedType, StructDef, TirLocal, TypeId, TypeTable};
use crate::token::Span;

/// Struct field info: module source and field definitions
#[derive(Clone)]
pub(crate) struct StructFieldInfo {
    /// Canonical type name (original declaration name, not import alias).
    pub(super) name: String,
    pub(super) module_source: ModuleSource,
    /// `AstId` of the `struct` declaration (`StructDecl::id`). The canonical
    /// identity: `TypeTable::type_of_symbol(defined_at)` gives the `TypeId`
    /// directly, without re-interning by `(name, module_source)`.
    pub(super) defined_at: AstId,
    /// Field definitions: (name, `type_id`, visibility) triples
    pub(super) fields: Vec<(String, TypeId, Visibility)>,
    /// Parallel array to `fields` holding each field's defining `AstId`.
    /// Used for recording use→def references (e.g. field access → field def).
    pub(super) field_ast_ids: Vec<AstId>,
    /// Default-value expressions, parallel to `fields`.
    /// `Some(expr)` means the field declared `= expr` and may be omitted at
    /// construction; `None` means the field is required.
    pub(super) field_defaults: Vec<Option<ast::Expr>>,
    /// The declaration's real type parameters. Bounds, defaults and arity are
    /// all read from here, never from a projection of it.
    pub(super) type_params: RealTypeParams,
    /// `TypeIds` of the struct's own type parameters in declaration order.
    /// Used by `infer_struct_type_args` to fill phantom type params
    /// (e.g., `D` in `struct DirMap<D, V>` where D doesn't appear in any field).
    pub(super) type_param_type_ids: Vec<TypeId>,
}

/// What a path read as a value names, when only a call can use it.
#[derive(Debug, Clone, Copy)]
pub enum CallableKind {
    Operation,
    StaticFunction,
}

/// Each slot whose default names its own or a later parameter, with the name.
pub(super) fn forward_type_param_defaults(params: &[ast::GenericParam]) -> Vec<(usize, String)> {
    params
        .iter()
        .enumerate()
        .filter_map(|(slot, param)| {
            let mut heads = Vec::new();
            collect_type_heads(param.default.as_ref()?, &mut heads);
            let (_, name) = heads
                .into_iter()
                .find(|(_, name)| params[slot..].iter().any(|p| p.name == *name))?;
            Some((slot, name))
        })
        .collect()
}

/// Every named head `ty` reaches, as a reference site and its spelling. The
/// scopeless twin of `Elaborator::walk_type_heads`: a `Self::` or `T::` prefix
/// is collected like any other and left to `declaration_at`, which answers
/// `None` for a binder.
fn collect_type_heads(ty: &ast::Type, out: &mut Vec<(AstId, String)>) {
    match ty {
        ast::Type::Named(named) => out.push((named.id, named.name.clone())),
        ast::Type::Generic(generic) => {
            out.push((generic.id, generic.name.clone()));
            for arg in &generic.args {
                collect_type_heads(arg, out);
            }
        }
        ast::Type::NamespacedGeneric(namespaced) => {
            out.push((namespaced.id, namespaced.name.clone()));
            for arg in &namespaced.args {
                collect_type_heads(arg, out);
            }
        }
        ast::Type::Function(func_ty) => {
            for param in &func_ty.params {
                collect_type_heads(param, out);
            }
            collect_type_heads(&func_ty.return_type, out);
        }
        ast::Type::Reference(inner) | ast::Type::MutReference(inner) => {
            collect_type_heads(inner, out);
        }
        ast::Type::Tuple(elements) => {
            for element in elements {
                collect_type_heads(element, out);
            }
        }
        ast::Type::TypePackSpread(_, _) | ast::Type::Infer(_) | ast::Type::Error(_) => {}
    }
}

/// Where a qualified prefix's members live when the prefix names a newtype,
/// paired with the type the prefix itself names. A newtype inherits its base's
/// members and keeps its own identity, so `C::Green` on `type C = Color` reads
/// Color's cases and yields a `C` — the implicit `Color::Green as C`. `None`
/// when the prefix names something that owns its members.
pub(super) fn newtype_member_owner(
    lookup: &TypeLookup<'_>,
    tysys: &TypeSystem,
    def: DefId,
) -> Option<(DefId, TypeId)> {
    let newtype_id = lookup.newtype_of(def)?;
    let head = tysys.type_table.borrow().reflect_structure_head(newtype_id);
    Some((tysys.type_def(head)?, newtype_id))
}

impl StructFieldInfo {
    /// `decl` with its fields resolved to `fields`, in declaration order.
    pub(crate) fn of_decl(
        module_source: ModuleSource,
        decl: &ast::StructDecl,
        fields: Vec<(String, TypeId, Visibility)>,
        type_param_type_ids: Vec<TypeId>,
    ) -> Self {
        Self {
            name: decl.name.clone(),
            module_source,
            defined_at: decl.id,
            fields,
            field_ast_ids: decl.fields.iter().map(|field| field.id).collect(),
            field_defaults: decl.fields.iter().map(|f| f.default.clone()).collect(),
            type_params: RealTypeParams::of(&decl.type_params),
            type_param_type_ids,
        }
    }

    /// Whether `Default` derives from the field defaults alone: every field
    /// declares one. A fieldless struct qualifies vacuously — it has exactly
    /// one value — which is what makes the `NoFields` marker a usable default
    /// for a type parameter. A generic struct does not: a default is
    /// elaborated against the declaration, not an instance.
    pub(super) fn auto_derives_default(&self) -> bool {
        self.type_param_type_ids.is_empty() && self.field_defaults.iter().all(Option::is_some)
    }

    /// Whether a reflection written in `module` can enumerate every field
    /// (WEP 2026-06-13, Visibility).
    pub(super) fn fields_visible_from(&self, module: &ModuleSource) -> bool {
        if self.fields.is_empty() || &self.module_source == module {
            return true;
        }
        let same_package = self.module_source.same_package(module);
        self.fields
            .iter()
            .all(|(_, _, vis)| vis.reachable_from(same_package))
    }
}

/// Variant case info: case name and payload type
#[derive(Clone)]
pub(crate) struct VariantCaseData {
    pub(super) name: String,
    /// Payload type for this case. Unit variants have `()` (unit type) payload.
    pub(crate) payload: TypeId,
    /// `AstId` of the case declaration (`VariantCase::id`) in the owning module.
    pub(crate) ast_id: AstId,
}

impl VariantCaseData {
    /// `decl`'s cases, each payload resolved by `payload_of`; a case without
    /// one carries `()`.
    pub(crate) fn collect(
        decl: &ast::VariantDecl,
        mut payload_of: impl FnMut(&ast::Type) -> TypeId,
    ) -> Vec<Self> {
        decl.cases
            .iter()
            .map(|case| Self {
                name: case.name.clone(),
                payload: case
                    .payload
                    .as_ref()
                    .map_or(TypeTable::UNIT, &mut payload_of),
                ast_id: case.id,
            })
            .collect()
    }

    /// Whether the case carries a payload, rather than being a unit case.
    pub(super) fn has_payload(&self, table: &TypeTable) -> bool {
        !matches!(table.get(self.payload), ResolvedType::Unit)
    }
}

/// Variant info: module source, type parameters, and cases
#[derive(Clone)]
pub(crate) struct VariantInfo {
    /// Canonical type name (original declaration name, not import alias).
    /// `pub(crate)` alongside `module_source` so the Semantics-based effect
    /// checker can key its payload map by the declaration, now that the
    /// registry no longer spells one out in its key.
    pub(crate) name: String,
    pub(crate) module_source: ModuleSource,
    /// `AstId` of the `variant` declaration (`VariantDecl::id`).
    pub(super) defined_at: AstId,
    /// The declaration's real type parameters, like
    /// [`StructFieldInfo::type_params`].
    pub(super) type_params: RealTypeParams,
    /// Per-case data. `pub(crate)` so the Semantics-based effect checker can
    /// follow resources nested in variant case payloads.
    pub(crate) cases: Vec<VariantCaseData>,
    /// `TypeIds` of the variant's own type parameters in declaration order.
    /// Used by `infer_variant_type_args` to fill type params from payload args
    /// and expected type context.
    pub(super) type_param_type_ids: Vec<TypeId>,
}

/// Enum case info: case name and discriminant index
#[derive(Clone)]
pub(super) struct EnumCaseData {
    pub(super) name: String,
    pub(super) index: u32,
    /// `AstId` of the case declaration (`EnumCase::id`) in the owning module.
    pub(super) ast_id: AstId,
}

/// Enum info: module source and cases (enums have no type parameters or payloads)
#[derive(Clone)]
pub(crate) struct EnumInfo {
    pub(super) module_source: ModuleSource,
    /// `AstId` of the `enum` declaration (`EnumDecl::id`).
    pub(super) defined_at: AstId,
    pub(super) cases: Vec<EnumCaseData>,
    /// O(1) lookup from case name to discriminant index
    pub(super) case_index: hashmap::IndexMap<String, u32>,
}

impl VariantInfo {
    pub(super) fn of_decl(
        module_source: ModuleSource,
        decl: &ast::VariantDecl,
        cases: Vec<VariantCaseData>,
        type_param_type_ids: Vec<TypeId>,
    ) -> Self {
        Self {
            name: decl.name.clone(),
            module_source,
            defined_at: decl.id,
            type_params: RealTypeParams::of(&decl.type_params),
            cases,
            type_param_type_ids,
        }
    }

    /// The case named `name`, with its position.
    pub(super) fn case_named(&self, name: &str) -> Option<(usize, &VariantCaseData)> {
        self.cases.iter().enumerate().find(|(_, c)| c.name == name)
    }
}

impl EnumInfo {
    pub(super) fn of_decl(module_source: ModuleSource, decl: &ast::EnumDecl) -> Self {
        let cases: Vec<EnumCaseData> = decl
            .cases
            .iter()
            .enumerate()
            .map(|(index, case)| EnumCaseData {
                name: case.name.clone(),
                index: index as u32,
                ast_id: case.id,
            })
            .collect();
        let case_index = cases.iter().map(|c| (c.name.clone(), c.index)).collect();
        Self {
            module_source,
            defined_at: decl.id,
            cases,
            case_index,
        }
    }

    /// O(1) case lookup by name
    pub(super) fn find_case(&self, name: &str) -> Option<&EnumCaseData> {
        let &idx = self.case_index.get(name)?;
        self.cases.iter().find(|c| c.index == idx)
    }
}

/// Flags member data: name and bitmask value
#[derive(Clone)]
pub(super) struct FlagsMemberData {
    pub(super) name: String,
    pub(super) bitmask: u32,
    /// `AstId` of the member declaration (`FlagsVariant::id`) in the owning module.
    pub(super) ast_id: AstId,
}

/// Flags type info: newtype `TypeId` and members
#[derive(Clone)]
pub(crate) struct FlagsInfo {
    pub(super) type_id: TypeId,
    pub(super) module_source: ModuleSource,
    pub(super) members: Vec<FlagsMemberData>,
}

impl FlagsInfo {
    /// The most members a declaration holds: each one's bitmask is `1 << index`.
    pub(super) const MAX_MEMBERS: usize = u32::BITS as usize;

    pub(super) fn of_decl(
        type_id: TypeId,
        module_source: ModuleSource,
        decl: &ast::FlagsDecl,
    ) -> Self {
        assert!(
            decl.flags.len() <= Self::MAX_MEMBERS,
            "a flags declaration wider than a word is rejected before this"
        );
        let members = decl
            .flags
            .iter()
            .enumerate()
            .map(|(i, m)| FlagsMemberData {
                name: m.name.clone(),
                bitmask: 1u32 << i,
                ast_id: m.id,
            })
            .collect();
        Self {
            type_id,
            module_source,
            members,
        }
    }
}

/// Resource info: module source and method names
/// Note: This infrastructure was added for resource static methods but isn't fully used yet.
/// Keep it for when wasi:sockets registration is re-enabled.
#[derive(Clone)]
pub(crate) struct ResourceInfo {
    /// Canonical type name (original declaration name, not import alias).
    pub(super) name: String,
    pub(super) module_source: ModuleSource,
    /// `AstId` of the `resource` declaration (`ResourceDecl::id`).
    pub(super) defined_at: AstId,
}

/// Generic newtype definition: `type Foo<T> = Bar<T>`
#[derive(Clone)]
pub(crate) struct GenericNewtypeInfo {
    /// The declaration's real type parameters, like
    /// [`StructFieldInfo::type_params`].
    pub(super) type_params: RealTypeParams,
    pub(super) base_type_ast: ast::Type,
}

impl GenericNewtypeInfo {
    pub(crate) fn of_decl(decl: &ast::Newtype) -> Self {
        Self {
            type_params: RealTypeParams::of(&decl.type_params),
            base_type_ast: decl.ty.clone(),
        }
    }
}

/// Which kind of inherent impl member a visibility violation names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImplMemberKind {
    Method,
    AssociatedConstant,
}

impl ImplMemberKind {
    fn noun(self) -> &'static str {
        match self {
            Self::Method => "method",
            Self::AssociatedConstant => "associated constant",
        }
    }

    fn verb(self) -> &'static str {
        match self {
            Self::Method => "called",
            Self::AssociatedConstant => "read",
        }
    }
}

/// The unlabeled jumps a loop binds.
#[derive(Debug, Clone, Copy)]
pub enum LoopJump {
    Break,
    Continue,
}

/// Which of a method's two parameter lists a diagnostic is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamList {
    Value,
    Type,
}

impl ParamList {
    fn prefix(self) -> &'static str {
        match self {
            ParamList::Value => "",
            ParamList::Type => "type ",
        }
    }
}

/// Errors from the type resolution phase
#[derive(Debug, Clone)]
pub enum TypeError {
    /// Type mismatch
    TypeMismatch {
        expected: String,
        found: String,
        span: Span,
    },

    /// A template passed to a tag whose parameter it cannot satisfy.
    TagParamNotTemplate {
        param: String,
        span: Span,
    },

    /// Unknown type name
    UnknownType {
        name: String,
        span: Span,
    },

    /// A bound writing `Self` where no declaration binds one.
    SelfInUnboundedBound {
        param: String,
        span: Span,
    },

    /// Expanding a type parameter's `= Default` reaches the declaration it
    /// belongs to again, so filling the slot has no fixpoint.
    RecursiveTypeParamDefault {
        name: String,
        span: Span,
    },

    /// A type parameter's `= Default` names a parameter declared at or after
    /// its own slot, which no argument has settled yet.
    ForwardTypeParamDefault {
        param: String,
        referenced: String,
        span: Span,
    },

    /// A type position names an `interface` or a `trait`. Both share the type
    /// namespace, and neither denotes a type.
    NotAType {
        name: String,
        /// What the declaration is, with its article: `an interface` or
        /// `a trait`.
        kind: &'static str,
        span: Span,
    },

    /// An `impl` head names a type the module does not declare. A block's own
    /// `impl<…>` list is the only way to introduce a type parameter.
    UndeclaredImplTypeParam {
        name: String,
        span: Span,
    },

    /// A `flags` declaration with more members than the CM ABI's single-word
    /// bitmask can represent (at most 32).
    FlagsTooManyMembers {
        name: String,
        count: usize,
        span: Span,
    },

    /// A by-value `self` receiver on a non-resource type. Only resources are
    /// move-only (WEP 2026-05-21); value types borrow with `&self`.
    SelfByValueOnNonResource {
        type_name: String,
        span: Span,
    },

    /// `_` inference placeholder used outside a turbofish type argument.
    InferPlaceholderNotAllowed {
        span: Span,
    },

    InferInLetAnnotation {
        span: Span,
    },

    MissingTypeArguments {
        name: String,
        expected: usize,
        span: Span,
    },

    /// A type application writing more arguments than the head declares.
    SurplusTypeArguments {
        name: String,
        expected: usize,
        found: usize,
        span: Span,
    },

    /// Unknown function
    UnknownFunction {
        name: String,
        span: Span,
    },

    /// A site named a declaration that reports a reason in place of a body.
    /// [`super::collect_unavailable`] renders the sentence.
    Unavailable {
        message: String,
        span: Span,
    },

    /// Unknown variable
    UnknownIdentifier {
        name: String,
        span: Span,
    },
    /// A path naming something only a call can use, read as a value.
    CallableAsValue {
        name: String,
        callable: CallableKind,
        span: Span,
    },

    /// `break LABEL` naming no enclosing labeled block
    UnknownBreakLabel {
        label: String,
        span: Span,
    },

    /// Unlabeled `break` / `continue` with no enclosing loop. WIR resolves such
    /// a jump against the innermost loop on its label stack and has none.
    LoopJumpOutsideLoop {
        jump: LoopJump,
        span: Span,
    },

    /// Wrong number of arguments
    ArgumentCountMismatch {
        expected: usize,
        found: usize,
        span: Span,
    },

    /// Expression in callee position whose type is not a function.
    CalleeNotCallable {
        type_name: String,
        span: Span,
    },

    /// A clause on an effect operation that dispatch cannot honour. `detail`
    /// names the clause and why: an operation's effects reach no call site, so
    /// a default body performing one would hand its caller a capability the
    /// caller never declared; and the dispatch record holds one slot per
    /// operation, not one per instantiation.
    OperationClauseNotAllowed {
        owner: String,
        operation: String,
        detail: &'static str,
        span: Span,
    },

    /// An attribute describing a declaration with no body, written on a trait
    /// or interface requirement; `reason` is its schema's `on_requirement`.
    AttributeOnRequirement {
        owner: String,
        operation: String,
        attribute: &'static str,
        reason: &'static str,
        span: Span,
    },

    /// Invalid numeric literal
    InvalidLiteral {
        message: String,
        span: Span,
    },

    /// A type could not be inferred and needs an explicit annotation
    /// (e.g. a bare `null` whose `Option<...>` inner is undetermined).
    CannotInferType {
        message: String,
        span: Span,
    },

    /// A bare case (`Red`) where no expected type supplies it: the type name
    /// may be omitted only where the context says which type is meant.
    BareCaseNeedsContext {
        case: String,
        /// The `Type::Case` spelling the site needs.
        qualified: String,
        /// The expected type, when there is one and it has no such case.
        expected: Option<String>,
        span: Span,
    },

    /// Invalid assignment target (not a valid l-value)
    CannotAssign {
        message: String,
        span: Span,
    },

    /// Mutation of a place rooted at an immutable reference (a `&mut self`
    /// method call or a field/index assignment through a `&T`)
    CannotMutate {
        message: String,
        span: Span,
    },

    /// Trait bound not satisfied
    TraitBoundNotSatisfied {
        type_name: String,
        trait_name: String,
        param_name: String,
        /// Reason chain explaining *why* `type_name` does not implement
        /// `trait_name`, walking the auto-derive structure step by step
        /// (e.g. "`Handler` does not implement `Ord` because field `cb` …").
        /// Empty when no structural explanation is available. Rendered as
        /// indented `note:` lines beneath the headline message.
        reason: Vec<String>,
        span: Span,
        /// Carries the enforcement's own proof, so this cannot be raised beside
        /// a hand-rolled bound check. See [`BoundUnmet`].
        unmet: BoundUnmet,
    },

    /// An explicit `impl Eq for T;` / `impl Ord for T;` marker was written,
    /// but `T` is not structurally eligible — some field/case does not
    /// itself implement the trait. Unlike [`Self::TraitBoundNotSatisfied`]
    /// (a bound unsatisfied at a use site), this always points at the
    /// marker's own span — the "guarantee" an explicit derive request is
    /// for (see `docs/wep-2026-06-25-trait-derivation.md`).
    ExplicitDeriveNotEligible {
        trait_name: String,
        type_name: String,
        reason: Vec<String>,
        span: Span,
    },

    /// An `impl Sub for T` block whose `T` does not implement one of `Sub`'s
    /// supertraits. Reported at the impl block: the impl is what promises the
    /// subtrait, so it is what owes the supertrait.
    SupertraitNotSatisfied {
        type_name: String,
        trait_name: String,
        supertrait: String,
        /// Reason chain, as in [`Self::TraitBoundNotSatisfied`].
        reason: Vec<String>,
        span: Span,
    },

    /// A method name reachable through more than one of a type parameter's
    /// bounds. Reported where it is called, not where the traits are
    /// declared: the same two traits are unambiguous on a receiver whose
    /// bounds name only one of them.
    AmbiguousTraitMethod {
        method: String,
        /// The bounds that declare it, in bound-list order.
        traits: Vec<String>,
        span: Span,
    },

    /// Two of one trait's value blankets whose bounds the receiver both
    /// satisfies, with nothing to rank them. A blanket has no name, so unlike
    /// [`Self::AmbiguousTraitMethod`] the call cannot pin one — only an impl
    /// written for the receiver answers it.
    AmbiguousValueBlankets {
        trait_name: String,
        receiver: String,
        /// The receiver-parameter bounds of each blanket, in declaration order.
        bounds: Vec<String>,
        span: Span,
    },

    /// An impl applies, and the calling module has imported none of the traits
    /// that would answer: the scope gate of WEP 2026-09-01. Names each trait,
    /// since importing one is the fix.
    TraitNotImported {
        method: String,
        receiver: String,
        traits: Vec<String>,
        span: Span,
    },

    /// An associated type name reachable through more than one of a type
    /// parameter's bounds, so `T::Output` names two types. Reported where it is
    /// written — the same two traits are unambiguous on another parameter.
    AmbiguousAssocType {
        assoc: String,
        param: String,
        /// The bounds that declare it, in bound-list order.
        traits: Vec<String>,
        span: Span,
    },

    /// One trait implemented for a receiver at two different argument lists,
    /// so a call on it names two signatures. Reported where it is called: the
    /// impls are individually fine, and coherence permits them.
    AmbiguousTraitArguments {
        method: String,
        /// The competing trait spellings, in candidate order.
        traits: Vec<String>,
        /// What each argument contributed to selection, in argument order.
        /// Empty when the call never reached argument-directed selection.
        arguments: Vec<String>,
        span: Span,
    },

    /// Every argument list of one trait rejected the arguments. The other end
    /// of [`TypeError::AmbiguousTraitArguments`]: selection had what it needed
    /// and no impl accepts it.
    NoMatchingOverload {
        method: String,
        /// The candidate spellings, in candidate order.
        traits: Vec<String>,
        /// What each argument contributed, in argument order.
        arguments: Vec<String>,
        span: Span,
    },

    /// An operator whose receiver implements its trait at several right-hand
    /// types, none of which the right operand selects. Same rule as a method
    /// call's argument lists, reported where the operator is written.
    AmbiguousOperatorRhs {
        op: String,
        type_name: String,
        /// The competing trait spellings (`Add<i32>`, `Add<Meters>`), in
        /// candidate order.
        candidates: Vec<String>,
        span: Span,
    },

    /// `Trait::method()` with no arguments: the trait-qualified call form
    /// takes the receiver as its first argument, so there is nothing to
    /// dispatch on.
    TraitQualifiedCallNeedsReceiver {
        trait_name: String,
        method: String,
        span: Span,
    },

    /// The receiver of a qualified call spells its own mode, and the spelled
    /// mode disagrees with the method's `self` parameter. Passing a value
    /// where the method takes `&mut self` would mutate a copy and silently
    /// drop the change — the case this error exists for.
    TraitQualifiedReceiverMode {
        trait_name: String,
        method: String,
        /// The method's `self` mode, rendered (`self`, `&self`, `&mut self`).
        expected: String,
        /// How to spell the receiver, rendered (`value`, `&value`,
        /// `&mut value`).
        spelled: String,
        span: Span,
    },

    /// A static reachable only through a blanket impl generic in the
    /// parameter it declares (`impl<T: Display> From<T> for Wrapper`).
    /// Selecting the instantiation from the argument's type is phase-4 work
    /// (WEP 2026-07-31); until then the call is rejected rather than reaching
    /// WIR build unresolved.
    UnsupportedBlanketInstantiation {
        trait_name: String,
        receiver: String,
        method: String,
        arg_type: String,
        span: Span,
    },

    /// A method's own type parameters that the arguments do not settle. Left
    /// unspelled they reach codegen unsubstituted.
    UninferredMethodTypeArgs {
        receiver: String,
        method: String,
        /// The method's own parameters the call must spell, in declaration
        /// order. Never empty.
        params: Vec<String>,
        span: Span,
    },

    /// `Trait::<T>::method()` on a trait declaring parameters of its own. The
    /// turbofish is that trait's argument list, so it names no receiver; a
    /// static's receiver is written out instead (`docs/spec.md`, "A trait's
    /// associated function").
    StaticNeedsWrittenReceiver {
        trait_name: String,
        method: String,
        span: Span,
    },

    /// A static call whose literal argument admits several impls
    /// (`Wrapper::from(42)` against `From<i32>` beside `From<i64>`). A literal
    /// never selects between the widths it could coerce to (WEP 2026-07-31),
    /// and a static has no `self`, so the trait turbofish escape does not
    /// apply — the fix is annotating the argument.
    AmbiguousStaticArgument {
        receiver: String,
        method: String,
        /// The admitted source types, in candidate order.
        candidates: Vec<String>,
        span: Span,
    },

    /// A static call whose receiver provides the method only through trait
    /// impls, none of which takes an argument of the call's type —
    /// `Wrapper::from(42)` against `From<String>` and `From<i64>`. Reported at
    /// the call: every impl is fine on its own, the argument matches none.
    NoMatchingTraitArgument {
        trait_name: String,
        receiver: String,
        method: String,
        arg_type: String,
        /// The argument types the impls do take, in candidate order.
        candidates: Vec<String>,
        span: Span,
    },

    /// An impl of the trait accepts the first argument, but none accepts the
    /// list. Distinct from [`Self::NoMatchingTraitArgument`], which is the
    /// first argument matching nothing at all.
    NoMatchingArgumentList {
        trait_name: String,
        receiver: String,
        method: String,
        span: Span,
    },

    /// A type argument whose associated type does not match the constraint
    /// written on the bound (`T: Collect<Item = i32>` given `Item = String`).
    AssocTypeBoundNotSatisfied {
        type_name: String,
        trait_name: String,
        assoc_name: String,
        expected: String,
        actual: String,
        span: Span,
    },

    /// A reflect member called on a type parameter whose bound binds no pack
    /// (`T: ReflectTemplate` without `Holes = [..V]`): the member reads one.
    MissingReflectPackBound {
        trait_name: String,
        type_param: String,
        method: String,
        pack_bound: String,
        span: Span,
    },

    /// A bound naming something that is not a declared trait.
    UnknownBound {
        name: String,
        span: Span,
    },

    /// A trait reaches itself through its supertrait clause, so no type could
    /// ever satisfy the obligation. `chain` is the path back to the trait,
    /// starting and ending at it (`A -> B -> A`).
    CircularSupertrait {
        trait_name: String,
        chain: Vec<String>,
        span: Span,
    },

    /// Invalid pattern in context
    InvalidPattern {
        message: String,
        span: Span,
    },

    /// A unary or binary operator cannot be applied to its operand type(s).
    ///
    /// Replaces the former practice of routing operator errors through
    /// `InvalidPattern`, which mislabeled them with an "invalid pattern:"
    /// prefix. `operands` holds one type name when both operands share a
    /// type (or for unary operators) and two names when a binary operator
    /// is applied to two differing types; `note` carries an optional
    /// explanation appended after a colon (e.g. "type does not implement
    /// `Sub`").
    OperatorNotApplicable {
        op: String,
        operands: Vec<String>,
        note: Option<String>,
        span: Span,
    },

    /// Invalid type cast
    InvalidCast {
        from: String,
        to: String,
        hint: String,
        span: Span,
    },

    /// Missing field in struct literal
    MissingField {
        struct_name: String,
        field_name: String,
        span: Span,
    },

    /// Extra field in struct literal (field does not exist on the struct)
    ExtraField {
        struct_name: String,
        field_name: String,
        span: Span,
    },

    /// Duplicate field name in struct literal
    DuplicateField {
        name: String,
        span: Span,
    },

    /// Function/method/closure with return type but no return statement
    MissingReturn {
        return_type: String,
        span: Span,
    },

    /// `export async fn` whose body carries no `task return`, so no call of it
    /// can ever finish its Component Model task.
    MissingTaskReturn {
        function: String,
        span: Span,
    },

    /// The else block of a `let ... else` does not diverge. It must exit the
    /// enclosing control flow (`return`, `break`, `continue`, `panic`, …) on
    /// every path, since it only runs when the refutable pattern fails to bind.
    LetElseMustDiverge {
        span: Span,
    },

    /// Orphan rule violation: impl of a foreign trait for a foreign type
    OrphanViolation {
        trait_name: String,
        self_type_name: String,
        span: Span,
    },

    /// Coherence violation: two variadic impls of the same trait accepting a
    /// common tuple. They apply at every such arity and bounds do not separate
    /// them.
    OverlappingVariadicImpls {
        trait_name: String,
        self_type_name: String,
        /// Where the impl this one collides with lives, named so the other
        /// half of the pair is not left for the reader to hunt down.
        conflicting_impl: String,
        span: Span,
    },

    /// Coherence violation: two impls of one `(Trait, Type)` pair. No rank
    /// separates them, so without this the collection order decides which body
    /// every call runs.
    DuplicateTraitImpl {
        trait_name: String,
        self_type_name: String,
        /// Where the impl this one duplicates lives.
        conflicting_impl: String,
        span: Span,
    },

    /// Coherence violation: `impl<T> Trait for T`. A value blanket is selected
    /// by its receiver parameter's bound, and this one states none.
    UnboundedValueBlanket {
        trait_name: String,
        param: String,
        span: Span,
    },

    /// Coherence violation: an inherent impl for one instantiation defines a
    /// method an inherent impl generic over the same head already defines.
    DuplicateInherentMethod {
        self_type_name: String,
        method_name: String,
        span: Span,
    },

    /// A variadic impl target whose pack is not the whole of it — beside other
    /// elements (`impl<..T> Trait for [i32, ..T]`) or under a reference
    /// (`&[..T]`) — which the compiler does not implement.
    UnsupportedVariadicImplTarget {
        span: Span,
    },

    /// A `..X` whose `X` is not a declared type pack. A scalar parameter stands
    /// for one position, so spreading it says nothing a bare `X` does not.
    SpreadOfNonPack {
        name: String,
        span: Span,
    },

    /// A turbofish spelling more than one type pack's arguments flat, which
    /// says nothing about where one pack ends and the next begins.
    UnspelledPackBoundary {
        span: Span,
    },

    /// A tuple `zip` whose rows are not all the same length. Two distinct packs
    /// are never known to be equally long, so the transpose has no answer.
    ZipOverUnequalPacks {
        row: String,
        span: Span,
    },

    /// An `impl` type parameter that its target and trait reference do not
    /// mention. Nothing at a use site says what such a parameter is: the
    /// receiver determines the ones the target names and no more, so the rest
    /// reach codegen unsubstituted. Rust rejects the same shape (E0207).
    UnconstrainedImplTypeParam {
        param_name: String,
        span: Span,
    },

    /// Coherence violation: an inherent `impl Type { ... }` on a foreign type
    /// (one defined outside this package — a primitive, `Array<T>`, `String`,
    /// or any other stdlib type). Inherent impls may only extend types owned by
    /// the current package; use a trait for cross-package extension.
    InherentImplOnForeignType {
        self_type_name: String,
        span: Span,
    },

    /// A sealed, compiler-synthesized trait (`ReflectStruct`) cannot be implemented
    /// in user code — the compiler provides its impl for every eligible type.
    SealedTraitImpl {
        trait_name: String,
        span: Span,
    },

    /// An `impl` method takes a different number of parameters than the trait
    /// declares. Left unreported it reaches codegen as a call built to the
    /// trait's arity against a body of another.
    TraitMethodArityMismatch {
        trait_name: String,
        method_name: String,
        list: ParamList,
        expected: usize,
        found: usize,
        span: Span,
    },

    /// An `impl` method's receiver disagrees with the trait's declaration. The
    /// same defect as an arity mismatch: no call site writes a receiver, so the
    /// call is built to the trait's shape and only fails Wasm validation.
    TraitMethodReceiverMismatch {
        trait_name: String,
        method_name: String,
        /// Whether the trait declares a receiver, and whether the impl does.
        expected: bool,
        found: bool,
        span: Span,
    },

    /// An `impl` leaves one of its trait's associated types unbound. None
    /// declares a default, so the projection reaches codegen unsubstituted.
    ImplMissingAssocType {
        trait_name: String,
        assoc_name: String,
        span: Span,
    },

    /// An `impl` binds a name its trait does not declare — a typo for one it
    /// does, which leaves the real associated type unbound.
    ImplAssocTypeNotInTrait {
        trait_name: String,
        assoc_name: String,
        span: Span,
    },

    /// An `impl` leaves a method its trait requires undefined. A trait method
    /// written with a body is a default and is not required.
    ImplMissingMethod {
        trait_name: String,
        method_name: String,
        span: Span,
    },

    /// `impl X for T` where `X` resolves to no trait, effect or resource in
    /// the impl's frame.
    UnknownTraitImpl {
        name: String,
        span: Span,
    },

    PrivateFieldAccess {
        struct_name: String,
        field_name: String,
        visibility: Visibility,
        span: Span,
    },

    /// A literal of a struct from elsewhere that leaves out fields it cannot set.
    HiddenFieldsOmitted {
        struct_name: String,
        field_names: Vec<String>,
        span: Span,
    },

    /// A module symbol named through a namespace path (`ns::item`) from
    /// beyond its declared visibility. The `use` form of the same reach is
    /// [`crate::analyze::AnalyzeError::SymbolNotVisible`]; both render the
    /// same sentence, since which syntax names the symbol does not change
    /// what bars it.
    PrivateNamespacedSymbol {
        name: String,
        module_source: ModuleSource,
        visibility: Visibility,
        span: Span,
    },

    /// An inherent impl member (method or associated constant) reached from
    /// beyond its declared visibility.
    PrivateMemberAccess {
        type_name: String,
        member_name: String,
        member_kind: ImplMemberKind,
        visibility: Visibility,
        span: Span,
    },

    /// Method not found on type
    MethodNotFound {
        type_name: String,
        method_name: String,
        hint: String,
        span: Span,
    },

    /// Invalid use of ? operator
    InvalidQuestionMark {
        message: String,
        span: Span,
    },

    /// Type does not implement required trait in usage context
    MissingTraitImpl {
        type_name: String,
        trait_name: String,
        span: Span,
    },

    /// A literal's target type admits more than one `From<Array<…>>`, so the
    /// literal alone does not say which conversion was meant.
    AmbiguousLiteralConversion {
        type_name: String,
        count: usize,
        span: Span,
    },

    /// Pattern expects different type kind (tuple, struct, variant, enum)
    PatternTypeMismatch {
        expected: String,
        found: String,
        span: Span,
    },

    /// `impl Trait for Type;` requested synthesis of a trait the compiler
    /// cannot generate. Only `From`, `Serialize`, `Deserialize`, `Eq`, and
    /// `Ord` are synthesizable through the bodyless-impl form.
    UnsupportedSynthesisTrait {
        trait_name: String,
        type_name: String,
        /// `true` when `trait_name` is the compiler's prelude `Display` (resolved
        /// scope-aware at the marker site, so a same-name user trait does not
        /// match) — gates the `Display`-specific "use `${x:?}`" hint.
        is_display: bool,
        span: Span,
    },

    /// Trait impl cannot re-specify a parameter default (defaults belong to
    /// the trait declaration).
    DefaultInTraitImpl {
        method: String,
        param: String,
        span: Span,
    },

    /// [`Self::DefaultInTraitImpl`] for a type parameter's default: the trait
    /// declares it and the call site fills it from there.
    TypeParamDefaultInTraitImpl {
        method: String,
        param: String,
        span: Span,
    },

    /// Closures cannot declare default parameter values. Closures erase
    /// defaults when assigned to a `fn(...)` type, so allowing them would
    /// be misleading — the default only ever applies to direct calls.
    DefaultInClosure {
        param: String,
        span: Span,
    },

    /// Calling a `fn mut` closure requires its binding to be `let mut` (for
    /// locals) or `mut name:` (for parameters). Mirrors Rust's `FnMut` rule.
    ClosureMutBindingRequired {
        name: String,
        span: Span,
    },

    /// Closures cannot cross the Component Model boundary. Triggered when an
    /// `export` (or imported) function has a closure-typed parameter, return
    /// type, or buried closure inside a container type.
    ClosureAtCmBoundary {
        function: String,
        position: String,
        span: Span,
    },

    /// A slice view has no Component Model representation. Triggered when an
    /// `export` (or imported) function has a slice-typed parameter or return
    /// type.
    SliceAtCmBoundary {
        function: String,
        position: String,
        span: Span,
    },

    /// `export fn` cannot declare default parameter values. The Component
    /// Model ABI requires every parameter at the CM boundary, so defaults
    /// would diverge from the WIT signature.
    DefaultInExportFn {
        function: String,
        param: String,
        span: Span,
    },

    /// `#[wire(default)]` is removed: a struct field default value is the
    /// single mechanism for an optional field. Guides to `field: T = <value>`.
    WireDefaultAttr {
        field: String,
        span: Span,
    },

    /// `resume` expression appeared outside an effect handler method body.
    /// `resume value` is only valid inside the body of a method belonging
    /// to an `impl Effect for Type` block (see WEP 2026-04-11).
    ResumeOutsideHandler {
        span: Span,
    },

    /// `with E => h do { ... }` clause where the handler value's type
    /// does not implement effect `E`.
    HandlerEffectNotImplemented {
        type_name: String,
        interface_name: String,
        span: Span,
    },

    AsyncUserEffectHandlerUnsupported {
        interface_name: String,
        op_name: String,
        span: Span,
    },

    AsyncOpMustReturnAsyncCall {
        op_name: String,
        span: Span,
    },

    /// Bundled-handler form `with &mut h do { ... }` where the handler value's
    /// underlying type does not implement any effect. There is nothing for
    /// `with h do` to install in this case — the user almost certainly meant
    /// to write `with E => h do` instead.
    BundledHandlerImplementsNoEffect {
        type_name: String,
        span: Span,
    },

    /// Bundled-handler form `with &mut h do { ... }` where the handler value's
    /// underlying type cannot be index-keyed by name (type parameters,
    /// associated-type projections, function types, ...). Bundled
    /// enumeration walks the impl-index by type name; these kinds have no
    /// stable name to look up. The explicit `with E => h do` form is the
    /// supported workaround.
    BundledHandlerUnsupportedHandlerType {
        type_name: String,
        type_kind: String,
        span: Span,
    },

    /// `with E => h do` clause where `E` is not a known effect or resource
    /// declaration (it might be a regular trait, an unrelated type, or an
    /// unknown name). Both kinds are installable as handlers; see WEP
    /// 2026-04-11.
    NotAnEffect {
        name: String,
        span: Span,
    },

    /// `with E => h do` where `E` is a generic effect parameter
    /// (`<effect E>`). Generic effect parameters are propagation-only:
    /// the compiler does not know `E`'s operation list at effect-check
    /// time, so it cannot generate dispatch infrastructure. See WEP
    /// 2026-01-27 § Generic Effect Parameters Are Propagation-Only.
    GenericEffectParamNotInstallable {
        name: String,
        span: Span,
    },

    /// `#[compiler_item("...")]` attribute that failed validation —
    /// unknown name, kind mismatch (e.g. `#[compiler_item("option")]`
    /// on a struct), or used outside a `core::*` module. The
    /// `message` carries the specific problem.
    CompilerItemAttr {
        message: String,
        span: Span,
    },

    /// One name reachable both as a resource's own or inherited method and
    /// through a visible trait impl. The call must name which it means.
    AmbiguousResourceMethod {
        method: String,
        resource: String,
        trait_name: String,
        span: Span,
    },

    /// A `resource Child extends Parent` clause the elaborator rejected: the
    /// parent is not a resource, a linearity does not match, the chain is
    /// cyclic, or the parent carries generic arguments.
    ResourceExtends {
        message: String,
        span: Span,
    },

    /// `#[cm(..., classes = ...)]` numbers an `extends` tree in a way a handle
    /// cannot be tested against, or a type pattern narrows to an unnumbered resource.
    ResourceClasses {
        message: String,
        span: Span,
    },

    /// A bare generic function name was used as a value with no expected
    /// `fn(...)` type to drive inference. The function type depends on
    /// type arguments that have not been supplied. The fix is to either
    /// pin them with turbofish (`name::<T, ...>`) or wrap the call in a
    /// closure (`|x| name(x)`) so the inner call site provides the
    /// inference context.
    BareGenericFunctionRef {
        name: String,
        span: Span,
    },

    /// Turbofish (`name::<T, ...>`) supplied the wrong number of type
    /// arguments for a generic function reference. The function declares
    /// `expected` type parameters; the user supplied `found`.
    GenericFunctionRefArgCountMismatch {
        name: String,
        expected: usize,
        found: usize,
        span: Span,
    },

    /// A bare reference to a generic function was used in a position with
    /// an expected `fn(...)` type, but the parameter counts disagree —
    /// the user almost certainly mis-counted arguments rather than mis-
    /// invoking turbofish. Surfacing the two arities directly is more
    /// helpful than the generic bare-reference message.
    GenericFunctionRefArityMismatch {
        name: String,
        expected_params: usize,
        found_params: usize,
        span: Span,
    },
}

/// Append a trait-bound reason chain as indented `note:` lines beneath a
/// headline message. Shared by the `Display` impl and the `Diagnostic`
/// conversion so the chain renders identically on every surface.
pub(super) fn append_reason_chain(mut message: String, reason: &[String]) -> String {
    for step in reason {
        message.push_str("\n  note: ");
        message.push_str(step);
    }
    message
}

pub(super) fn format_operator_not_applicable(
    op: &str,
    operands: &[String],
    note: Option<&str>,
) -> String {
    let base = match operands {
        [t] => format!("operator `{op}` cannot be applied to type `{t}`"),
        [l, r] => format!("operator `{op}` cannot be applied to types `{l}` and `{r}`"),
        _ => format!("operator `{op}` cannot be applied"),
    };
    match note {
        Some(note) => format!("{base}: {note}"),
        None => base,
    }
}

impl TypeError {
    /// The one sentence every use site says when a type parameter goes
    /// unanswered. `owner` names the declaration and `turbofish` shows the
    /// spelling that would answer it, both already backticked.
    pub(super) fn cannot_infer(
        names: &[String],
        owner: &str,
        turbofish: &str,
        span: Span,
    ) -> TypeError {
        let named = names
            .iter()
            .map(|n| format!("`{n}`"))
            .collect::<Vec<_>>()
            .join(", ");
        TypeError::CannotInferType {
            message: format!(
                "cannot infer type parameter {named} of {owner}; \
                 add a turbofish ({turbofish}) or a type annotation"
            ),
            span,
        }
    }

    /// This error as its `(code, message, span)` triple, which is what
    /// `From<TypeError> for Diagnostic` fills a `Diagnostic` from.
    pub(super) fn render(&self) -> (Code, String, Span) {
        use crate::compiler_host::Code;
        match self {
            TypeError::TypeMismatch {
                expected,
                found,
                span,
            } => (
                Code::TypeMismatch,
                format!("type mismatch: expected '{expected}', found '{found}'"),
                *span,
            ),
            TypeError::TagParamNotTemplate { param, span } => (
                Code::TypeMismatch,
                format!("a template tag's parameter must be bound by `ReflectTemplate`, not `{param}`"),
                *span,
            ),
            TypeError::UnknownType { name, span } => (
                Code::UnknownType,
                format!("unknown type '{}'", unalias_namespace_member(name)),
                *span,
            ),
            TypeError::SelfInUnboundedBound { param, span } => (
                Code::UnknownType,
                format!(
                    "`Self` in a bound on `{param}` names no implementing type here; write `{param}`"
                ),
                *span,
            ),
            TypeError::RecursiveTypeParamDefault { name, span } => (
                Code::UnknownType,
                format!(
                    "the default for this type parameter expands into '{name}' again, the \
                     declaration it belongs to, so filling the slot never settles"
                ),
                *span,
            ),
            TypeError::ForwardTypeParamDefault {
                param,
                referenced,
                span,
            } => (
                Code::UnknownType,
                format!(
                    "the default for type parameter '{param}' names '{referenced}', which is \
                     declared no earlier than '{param}' itself: only a parameter to its left \
                     has an argument to stand for"
                ),
                *span,
            ),
            TypeError::NotAType { name, kind, span } => (
                Code::UnknownType,
                format!(
                    "`{}` is {kind}, not a type: it names a set of operations, \
                     and no value has it as its type",
                    unalias_namespace_member(name)
                ),
                *span,
            ),
            TypeError::UndeclaredImplTypeParam { name, span } => (
                Code::UnknownType,
                format!(
                    "unknown type '{name}' in the impl target; declare it as a type parameter ('impl<{name}> …') or spell a type the module declares"
                ),
                *span,
            ),
            TypeError::FlagsTooManyMembers { name, count, span } => (
                Code::UnsupportedFeature,
                format!(
                    "flags `{name}` has {count} members; at most 32 are supported \
                     (a flags value is a single 32-bit word at the Component Model boundary)"
                ),
                *span,
            ),
            TypeError::SelfByValueOnNonResource { type_name, span } => (
                Code::ReceiverMismatch,
                format!(
                    "`self` by value is only allowed on a resource; `{type_name}` is a value type — use `&self`"
                ),
                *span,
            ),
            TypeError::InferPlaceholderNotAllowed { span } => (
                Code::UnknownType,
                "`_` type placeholder is only allowed as a turbofish type argument".to_string(),
                *span,
            ),
            TypeError::InferInLetAnnotation { span } => (
                Code::UnknownType,
                "type inference placeholder `_` in a `let` annotation is not yet supported; \
                 write the type explicitly (e.g. `Option<u32>`) or omit the annotation"
                    .to_string(),
                *span,
            ),
            TypeError::MissingTypeArguments {
                name,
                expected,
                span,
            } => (
                Code::ArityMismatch,
                {
                    let name = unalias_namespace_member(name);
                    format!(
                        "missing type arguments for `{name}`: expected {expected} type argument{}; \
                         supply them (e.g. `{name}<...>`) or drop the annotation to infer from the initializer",
                        if *expected == 1 { "" } else { "s" },
                    )
                },
                *span,
            ),
            TypeError::SurplusTypeArguments {
                name,
                expected,
                found,
                span,
            } => (
                Code::ArityMismatch,
                {
                    // Surplus, so a declared parameter puts `found` at two or
                    // more and only the `expected == 0` wording needs "was".
                    debug_assert!(found > expected);
                    let name = unalias_namespace_member(name);
                    if *expected == 0 {
                        format!(
                            "`{name}` takes no type arguments, but {found} {} supplied",
                            if *found == 1 { "was" } else { "were" },
                        )
                    } else {
                        format!(
                            "`{name}` takes {expected} type argument{}, but {found} were supplied",
                            if *expected == 1 { "" } else { "s" },
                        )
                    }
                },
                *span,
            ),
            TypeError::UnknownFunction { name, span } => (
                Code::UndefinedVariable,
                format!("unknown function '{}'", unalias_namespace_member(name)),
                *span,
            ),
            TypeError::Unavailable { message, span } => {
                (Code::Unavailable, message.clone(), *span)
            }
            TypeError::UnknownIdentifier { name, span } => (
                Code::UndefinedVariable,
                format!("unknown identifier '{}'", unalias_namespace_member(name)),
                *span,
            ),
            TypeError::CallableAsValue {
                name,
                callable,
                span,
            } => {
                let what = match callable {
                    CallableKind::Operation => "an operation",
                    CallableKind::StaticFunction => "a static function",
                };
                (
                    Code::UndefinedVariable,
                    format!(
                        "`{}` is {what}: call it rather than read it as a value",
                        unalias_namespace_member(name)
                    ),
                    *span,
                )
            }
            TypeError::UnknownBreakLabel { label, span } => (
                Code::UndefinedVariable,
                format!("labeled break target not found: no enclosing block labeled '{label}'"),
                *span,
            ),
            TypeError::LoopJumpOutsideLoop { jump, span } => (
                Code::InvalidSyntax,
                match jump {
                    LoopJump::Break => {
                        "`break` outside of a loop; a labeled block is left with `break LABEL`"
                            .to_string()
                    }
                    LoopJump::Continue => "`continue` outside of a loop".to_string(),
                },
                *span,
            ),

            TypeError::ArgumentCountMismatch {
                expected,
                found,
                span,
            } => (
                Code::ArityMismatch,
                format!("expected {expected} arguments, found {found}"),
                *span,
            ),
            TypeError::CalleeNotCallable { type_name, span } => (
                Code::NotCallable,
                format!("expression is not callable: type '{type_name}' is not a function"),
                *span,
            ),
            TypeError::OperationClauseNotAllowed {
                owner,
                operation,
                detail,
                span,
            } => (
                Code::InvalidSyntax,
                format!("`{owner}::{operation}` {detail}"),
                *span,
            ),
            TypeError::AttributeOnRequirement {
                owner,
                operation,
                attribute,
                reason,
                span,
            } => (
                Code::InvalidSyntax,
                format!("`{owner}::{operation}` cannot declare `#[{attribute}]`: {reason}"),
                *span,
            ),
            TypeError::InvalidLiteral { message, span } => {
                (Code::InvalidSyntax, message.clone(), *span)
            }
            TypeError::CannotInferType { message, span } => {
                (Code::NeedsTypeAnnotation, message.clone(), *span)
            }
            TypeError::BareCaseNeedsContext {
                case,
                qualified,
                expected,
                span,
            } => (
                Code::NeedsTypeAnnotation,
                {
                    let context = match expected {
                        Some(expected) => {
                            format!("the expected type `{expected}` has none by that name")
                        }
                        None => "nothing here says of which type".to_string(),
                    };
                    format!("`{case}` is a case, and {context}; write `{qualified}`")
                },
                *span,
            ),

            TypeError::CannotAssign { message, span } => (
                Code::ImmutableAssignment,
                format!("cannot assign: {message}"),
                *span,
            ),
            TypeError::CannotMutate { message, span } => {
                (Code::ImmutableAssignment, message.clone(), *span)
            }
            TypeError::TraitBoundNotSatisfied {
                type_name,
                trait_name,
                param_name,
                reason,
                span,
                unmet: _,
            } => (
                Code::TraitBoundNotSatisfied,
                append_reason_chain(
                    format!(
                        "type '{type_name}' does not implement trait '{trait_name}' required by bound on '{param_name}'"
                    ),
                    reason,
                ),
                *span,
            ),
            TypeError::SupertraitNotSatisfied {
                type_name,
                trait_name,
                supertrait,
                reason,
                span,
            } => (
                Code::TraitBoundNotSatisfied,
                append_reason_chain(
                    format!(
                        "type '{type_name}' implements trait '{trait_name}' but not its supertrait '{supertrait}'"
                    ),
                    reason,
                ),
                *span,
            ),
            TypeError::AmbiguousTraitArguments {
                method,
                traits,
                arguments,
                span,
            } => (
                Code::AmbiguousCandidate,
                append_reason_chain(
                    format!(
                        "ambiguous call to '{method}': the arguments do not select between {}; annotate an argument (e.g. '42 as i64') or pin the trait, e.g. '{}::{method}(&value, …)'",
                        traits
                            .iter()
                            .map(|t| format!("'{t}'"))
                            .collect::<Vec<_>>()
                            .join(" and "),
                        traits
                            .first()
                            .map(|t| t.replacen('<', "::<", 1))
                            .expect("ambiguity reported with no candidate traits")
                    ),
                    arguments,
                ),
                *span,
            ),
            TypeError::NoMatchingOverload {
                method,
                traits,
                arguments,
                span,
            } => (
                Code::TypeMismatch,
                append_reason_chain(
                    format!(
                        "no overload of '{method}' accepts these arguments: the candidates are {}",
                        traits
                            .iter()
                            .map(|t| format!("'{t}'"))
                            .collect::<Vec<_>>()
                            .join(" and ")
                    ),
                    arguments,
                ),
                *span,
            ),
            TypeError::AmbiguousOperatorRhs {
                op,
                type_name,
                candidates,
                span,
            } => (
                Code::AmbiguousCandidate,
                format!(
                    "ambiguous operator `{op}` on '{type_name}': the right operand does not select between {}; annotate it (e.g. '42 as i64') to pin one",
                    candidates
                        .iter()
                        .map(|c| format!("'{c}'"))
                        .collect::<Vec<_>>()
                        .join(" and ")
                ),
                *span,
            ),
            TypeError::AmbiguousAssocType {
                assoc,
                param,
                traits,
                span,
            } => (
                Code::AmbiguousCandidate,
                format!(
                    "ambiguous associated type '{param}::{assoc}': declared by {}; name the trait's own binding, e.g. '{param}: {}<{assoc} = ...>'",
                    traits
                        .iter()
                        .map(|t| format!("'{t}'"))
                        .collect::<Vec<_>>()
                        .join(" and "),
                    traits.first().map(String::as_str).unwrap_or(assoc)
                ),
                *span,
            ),
            TypeError::AmbiguousValueBlankets {
                trait_name,
                receiver,
                bounds,
                span,
            } => (
                Code::AmbiguousCandidate,
                format!(
                    "ambiguous blanket impls of '{trait_name}' for '{receiver}': {} apply, and nothing ranks them; write 'impl {trait_name} for {receiver}'",
                    bounds
                        .iter()
                        .map(|b| format!("'{b}'"))
                        .collect::<Vec<_>>()
                        .join(" and "),
                ),
                *span,
            ),
            TypeError::TraitNotImported {
                method,
                receiver,
                traits,
                span,
            } => (
                Code::TraitNotImported,
                format!(
                    "{} is not imported here: '{receiver}' has '{method}' through it; import the trait to call it",
                    traits
                        .iter()
                        .map(|t| format!("'{t}'"))
                        .collect::<Vec<_>>()
                        .join(" or "),
                ),
                *span,
            ),
            TypeError::AmbiguousTraitMethod {
                method,
                traits,
                span,
            } => (
                Code::AmbiguousCandidate,
                format!(
                    "ambiguous method '{method}': declared by {}; name the trait, e.g. '{}::{method}(&value)'",
                    traits
                        .iter()
                        .map(|t| format!("'{t}'"))
                        .collect::<Vec<_>>()
                        .join(" and "),
                    traits
                        .first()
                        .map(String::as_str)
                        .map(|t| t.split(" (from ").next().unwrap_or(t))
                        .expect("ambiguity reported with no candidate traits")
                ),
                *span,
            ),
            TypeError::TraitQualifiedCallNeedsReceiver {
                trait_name,
                method,
                span,
            } => (
                Code::ReceiverMismatch,
                format!(
                    "'{trait_name}::{method}' takes the receiver as its first argument, e.g. '{trait_name}::{method}(&value)'"
                ),
                *span,
            ),
            TypeError::TraitQualifiedReceiverMode {
                trait_name,
                method,
                expected,
                spelled,
                span,
            } => (
                Code::ReceiverMismatch,
                format!(
                    "'{trait_name}::{method}' takes '{expected}'; the receiver spells its own mode: '{trait_name}::{method}({spelled}, …)'"
                ),
                *span,
            ),
            TypeError::UninferredMethodTypeArgs {
                receiver,
                method,
                params,
                span,
            } => (
                Code::NeedsTypeAnnotation,
                {
                    assert!(!params.is_empty(), "the emitter found an unspelled slot");
                    let named = params
                        .iter()
                        .map(|p| format!("'{p}'"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let (plural, verb, them) = if params.len() == 1 {
                        ("", "is", "it")
                    } else {
                        ("s", "are", "them")
                    };
                    let spelled = params.join(", ");
                    format!(
                        "'{receiver}::{method}' declares the type parameter{plural} {named}, which {verb} not inferred from the arguments here; spell {them}: '{receiver}::{method}::<{spelled}>(…)'"
                    )
                },
                *span,
            ),
            TypeError::AmbiguousStaticArgument {
                receiver,
                method,
                candidates,
                span,
            } => (
                Code::AmbiguousCandidate,
                format!(
                    "ambiguous call to '{receiver}::{method}': a literal argument admits {}; annotate the argument (e.g. '42 as i64')",
                    candidates
                        .iter()
                        .map(|c| format!("'{c}'"))
                        .collect::<Vec<_>>()
                        .join(" and ")
                ),
                *span,
            ),
            TypeError::UnsupportedBlanketInstantiation {
                trait_name,
                receiver,
                method,
                arg_type,
                span,
            } => (
                Code::TraitDeclInvalid,
                format!(
                    "'{receiver}::{method}' resolves to a blanket '{trait_name}' impl, and selecting its instantiation from the argument is not supported yet; write a concrete 'impl {trait_name}<{arg_type}> for {receiver}'"
                ),
                *span,
            ),
            TypeError::StaticNeedsWrittenReceiver {
                trait_name,
                method,
                span,
            } => (
                Code::NeedsTypeAnnotation,
                format!(
                    "'{trait_name}' declares type parameters of its own, so the turbofish of '{trait_name}::<…>::{method}' is its argument list and names no receiver; write the receiver out ('Receiver::{method}(…)'), where the arguments select the impl"
                ),
                *span,
            ),
            TypeError::NoMatchingTraitArgument {
                trait_name,
                receiver,
                method,
                arg_type,
                candidates,
                span,
            } => (
                Code::TypeMismatch,
                format!(
                    "no impl of '{trait_name}' for '{receiver}' takes an argument of type '{arg_type}'; '{receiver}::{method}' is available for {}",
                    candidates
                        .iter()
                        .map(|c| format!("'{c}'"))
                        .collect::<Vec<_>>()
                        .join(" and ")
                ),
                *span,
            ),
            TypeError::NoMatchingArgumentList {
                trait_name,
                receiver,
                method,
                span,
            } => (
                Code::TypeMismatch,
                format!(
                    "no impl of '{trait_name}' for '{receiver}' declares '{method}' for these arguments"
                ),
                *span,
            ),
            TypeError::AssocTypeBoundNotSatisfied {
                type_name,
                trait_name,
                assoc_name,
                expected,
                actual,
                span,
            } => (
                Code::TraitBoundNotSatisfied,
                format!(
                    "type '{type_name}' does not satisfy the associated type '{trait_name}::{assoc_name} = {expected}': it is '{actual}'"
                ),
                *span,
            ),
            TypeError::MissingReflectPackBound {
                trait_name,
                type_param,
                method,
                pack_bound,
                span,
            } => (
                Code::TraitBoundNotSatisfied,
                format!(
                    "`{trait_name}::<{type_param}>::{method}()` needs `{type_param}` bound as \
                     `{trait_name}<{pack_bound}>`"
                ),
                *span,
            ),
            TypeError::UnknownBound { name, span } => (
                Code::UnknownType,
                format!("'{}' is not a declared trait", unalias_namespace_member(name)),
                *span,
            ),
            TypeError::CircularSupertrait {
                trait_name,
                chain,
                span,
            } => (
                Code::TraitDeclInvalid,
                format!(
                    "circular supertrait: trait '{trait_name}' is its own supertrait via {}",
                    chain.join(" -> ")
                ),
                *span,
            ),
            TypeError::ExplicitDeriveNotEligible {
                trait_name,
                type_name,
                reason,
                span,
            } => (
                Code::TraitDeclInvalid,
                append_reason_chain(
                    format!(
                        "cannot derive `{trait_name}` for `{type_name}`: not every field/case implements `{trait_name}`"
                    ),
                    reason,
                ),
                *span,
            ),
            TypeError::InvalidPattern { message, span } => (
                Code::InvalidSyntax,
                format!("invalid pattern: {message}"),
                *span,
            ),
            TypeError::OperatorNotApplicable {
                op,
                operands,
                note,
                span,
            } => (
                Code::TypeMismatch,
                format_operator_not_applicable(op, operands, note.as_deref()),
                *span,
            ),
            TypeError::InvalidCast {
                from,
                to,
                hint,
                span,
            } => (
                Code::InvalidCast,
                format!("cannot cast '{from}' to '{to}': {hint}"),
                *span,
            ),
            TypeError::MissingField {
                struct_name,
                field_name,
                span,
            } => (
                Code::StructFieldMismatch,
                format!("missing field '{field_name}' in struct literal '{struct_name}'"),
                *span,
            ),
            TypeError::ExtraField {
                struct_name,
                field_name,
                span,
            } => (
                Code::StructFieldMismatch,
                format!("struct '{struct_name}' has no field '{field_name}'"),
                *span,
            ),
            TypeError::DuplicateField { name, span } => (
                Code::StructFieldMismatch,
                format!("duplicate field '{name}' in struct literal"),
                *span,
            ),
            TypeError::MissingReturn { return_type, span } => (
                Code::MissingReturn,
                format!("function with return type '{return_type}' must use explicit `return`"),
                *span,
            ),
            TypeError::MissingTaskReturn { function, span } => (
                Code::MissingReturn,
                format!(
                    "`export async fn {function}` never delivers its result; \
                     add a `task return` where the result is ready"
                ),
                *span,
            ),
            TypeError::LetElseMustDiverge { span } => (
                Code::MissingReturn,
                "the `else` block of a `let ... else` must diverge (`return`, `break`, \
                 `continue`, `panic`, …)"
                    .to_string(),
                *span,
            ),
            TypeError::OrphanViolation {
                trait_name,
                self_type_name,
                span,
            } => (
                Code::OrphanRule,
                format!(
                    "orphan rule violation: cannot implement foreign trait `{trait_name}` for foreign type `{self_type_name}`"
                ),
                *span,
            ),
            TypeError::OverlappingVariadicImpls {
                trait_name,
                self_type_name,
                conflicting_impl,
                span,
            } => (
                Code::OrphanRule,
                format!(
                    "overlapping variadic impls of `{trait_name}` for `{self_type_name}`: this one and {conflicting_impl} accept the same tuples, and a pack's bounds are only checked at monomorphization, so neither can be selected over the other"
                ),
                *span,
            ),
            TypeError::DuplicateTraitImpl {
                trait_name,
                self_type_name,
                conflicting_impl,
                span,
            } => (
                Code::OrphanRule,
                format!(
                    "duplicate impl of `{trait_name}` for `{self_type_name}`: {conflicting_impl} implements the same pair, and nothing ranks two impls of one pair, so which one every call runs would be decided by the order they were loaded in"
                ),
                *span,
            ),
            TypeError::UnboundedValueBlanket {
                trait_name,
                param,
                span,
            } => (
                Code::OrphanRule,
                format!(
                    "blanket impl of `{trait_name}` for `{param}` states no bound: a blanket impl's receiver type parameter needs a bound, since the bound is what decides which receivers it covers"
                ),
                *span,
            ),
            TypeError::DuplicateInherentMethod {
                self_type_name,
                method_name,
                span,
            } => (
                Code::OrphanRule,
                format!(
                    "duplicate definition of `{method_name}` for `{self_type_name}`: another inherent impl already defines it, and an inherent impl carries no trait contract to make the two interchangeable"
                ),
                *span,
            ),
            TypeError::UnsupportedVariadicImplTarget { span } => (
                Code::OrphanRule,
                "a variadic impl target must be the bare `[..T]`: a pack alongside other elements (`[i32, ..T]`) or under a reference (`&[..T]`) is not supported yet".to_string(),
                *span,
            ),
            TypeError::SpreadOfNonPack { name, span } => (
                Code::TypeMismatch,
                format!(
                    "`..{name}` spreads `{name}`, which is not a type pack: declare it as `..{name}` in the type parameter list, or write `{name}` here"
                ),
                *span,
            ),
            TypeError::ZipOverUnequalPacks { row, span } => (
                Code::TypeMismatch,
                format!(
                    "`zip` transposes its rows position by position, so every row must be the same length; `{row}` is not the length of the first. Two type packs are never known to be equally long, so `zip` over them is not supported"
                ),
                *span,
            ),
            TypeError::UnspelledPackBoundary { span } => (
                Code::TypeMismatch,
                "with more than one type pack, a flat list of type arguments does not say where one pack ends; spell each type pack as a tuple, as in `f::<[i32], [bool]>(...)`".to_string(),
                *span,
            ),
            TypeError::UnconstrainedImplTypeParam { param_name, span } => (
                Code::TraitDeclInvalid,
                format!(
                    "the type parameter `{param_name}` is not constrained by the impl target or the trait reference"
                ),
                *span,
            ),
            TypeError::InherentImplOnForeignType {
                self_type_name,
                span,
            } => (
                Code::OrphanRule,
                format!(
                    "coherence violation: cannot define an inherent `impl` on foreign type `{self_type_name}` (defined in another package); use a trait to extend it"
                ),
                *span,
            ),
            TypeError::SealedTraitImpl { trait_name, span } => (
                Code::OrphanRule,
                format!(
                    "cannot implement `{trait_name}`: it is a sealed, compiler-synthesized trait provided automatically for every eligible type"
                ),
                *span,
            ),
            TypeError::TraitMethodArityMismatch {
                trait_name,
                method_name,
                list,
                expected,
                found,
                span,
            } => (
                Code::ArityMismatch,
                format!(
                    "method `{method_name}` takes {found} {}parameter(s) but `{trait_name}` declares {expected}",
                    list.prefix()
                ),
                *span,
            ),
            TypeError::TraitMethodReceiverMismatch {
                trait_name,
                method_name,
                expected,
                found,
                span,
            } => (
                Code::ReceiverMismatch,
                {
                    let receiver = |has: bool| if has { "a receiver" } else { "no receiver" };
                    format!(
                        "method `{method_name}` takes {} but `{trait_name}` declares {}",
                        receiver(*found),
                        receiver(*expected)
                    )
                },
                *span,
            ),
            TypeError::ImplMissingAssocType {
                trait_name,
                assoc_name,
                span,
            } => (
                Code::TraitDeclInvalid,
                format!("impl of trait '{trait_name}' does not bind associated type '{assoc_name}'"),
                *span,
            ),
            TypeError::ImplAssocTypeNotInTrait {
                trait_name,
                assoc_name,
                span,
            } => (
                Code::TraitDeclInvalid,
                format!("trait '{trait_name}' declares no associated type '{assoc_name}'"),
                *span,
            ),
            TypeError::ImplMissingMethod {
                trait_name,
                method_name,
                span,
            } => (
                Code::TraitDeclInvalid,
                format!("impl of trait '{trait_name}' does not define method '{method_name}'"),
                *span,
            ),
            TypeError::UnknownTraitImpl { name, span } => (
                Code::UnknownType,
                format!(
                    "cannot implement `{}`: no trait, effect or resource by that name is in scope",
                    unalias_namespace_member(name)
                ),
                *span,
            ),
            TypeError::PrivateFieldAccess {
                struct_name,
                field_name,
                visibility,
                span,
            } => (
                Code::PrivateSymbol,
                match visibility {
                    Visibility::Internal => format!(
                        "field `{field_name}` of struct `{struct_name}` is `internal` to its \
                         package and cannot be accessed from another package; mark it `pub` to \
                         expose it across packages"
                    ),
                    Visibility::Private => format!(
                        "field `{field_name}` of struct `{struct_name}` is private to its defining \
                         file; mark it `internal` (same package) or `pub` (cross package) to widen \
                         access"
                    ),
                    Visibility::Public => {
                        unreachable!("a `pub` field is reachable from every module")
                    }
                },
                *span,
            ),
            TypeError::HiddenFieldsOmitted {
                struct_name,
                field_names,
                span,
            } => (
                Code::PrivateSymbol,
                format!(
                    "cannot build `{struct_name}` with a literal here: it has fields this module \
                     cannot set ({}); call a constructor it provides",
                    field_names
                        .iter()
                        .map(|name| format!("`{name}`"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                *span,
            ),
            TypeError::PrivateNamespacedSymbol {
                name,
                module_source,
                visibility,
                span,
            } => (
                Code::PrivateSymbol,
                symbol_not_visible_message(name, module_source, *visibility),
                *span,
            ),
            TypeError::PrivateMemberAccess {
                type_name,
                member_name,
                member_kind,
                visibility,
                span,
            } => (
                Code::PrivateSymbol,
                {
                    let kind = member_kind.noun();
                    let verb = member_kind.verb();
                    match visibility {
                        Visibility::Internal => format!(
                            "{kind} `{member_name}` of `{type_name}` is `internal` to its package \
                             and cannot be {verb} from another package; mark it `pub` to \
                             expose it across packages"
                        ),
                        Visibility::Private | Visibility::Public => {
                            format!(
                                "{kind} `{member_name}` of `{type_name}` is private to its defining \
                                 file; mark it `internal` (same package) or `pub` (cross \
                                 package) to widen access"
                            )
                        }
                    }
                },
                *span,
            ),
            TypeError::MethodNotFound {
                type_name,
                method_name,
                hint,
                span,
            } => (
                Code::MethodNotFound,
                if hint.is_empty() {
                    format!("no method '{method_name}' found on type '{type_name}'")
                } else {
                    format!("no method '{method_name}' found on type '{type_name}'; {hint}")
                },
                *span,
            ),
            TypeError::InvalidQuestionMark { message, span } => {
                (Code::TypeMismatch, message.clone(), *span)
            }
            TypeError::MissingTraitImpl {
                type_name,
                trait_name,
                span,
            } => (
                Code::TraitBoundNotSatisfied,
                format!("type '{type_name}' does not implement {trait_name}"),
                *span,
            ),
            TypeError::AmbiguousLiteralConversion {
                type_name,
                count,
                span,
            } => (
                Code::AmbiguousCandidate,
                format!(
                    "literal is ambiguous: '{type_name}' has {count} `From<Array<…>>` impls that \
                     could build it — write `{type_name}::from(…)` to choose one"
                ),
                *span,
            ),
            TypeError::PatternTypeMismatch {
                expected,
                found,
                span,
            } => (
                Code::TypeMismatch,
                format!("pattern mismatch: expected '{expected}', found '{found}'"),
                *span,
            ),
            TypeError::UnsupportedSynthesisTrait {
                trait_name,
                type_name,
                is_display,
                span,
            } => {
                let hint = if *is_display {
                    " `Display` is never auto-derived (plain enums display their bare case name automatically); write a manual `impl Display`, or use `${x:?}` for debug output"
                } else {
                    ""
                };
                (
                    Code::TraitDeclInvalid,
                    format!(
                        "cannot synthesize trait `{trait_name}` for `{type_name}`: `impl Trait for Type;` is supported for `From`, `Serialize`, `Deserialize`, `Eq`, `Ord`, `Default`, and `Inspect`.{hint}"
                    ),
                    *span,
                )
            }
            TypeError::DefaultInTraitImpl {
                method,
                param,
                span,
            } => (
                Code::TraitDeclInvalid,
                format!(
                    "default value for parameter '{param}' in method '{method}' is not allowed; defaults belong to the trait declaration"
                ),
                *span,
            ),
            TypeError::TypeParamDefaultInTraitImpl {
                method,
                param,
                span,
            } => (
                Code::TraitDeclInvalid,
                format!(
                    "default for type parameter '{param}' in method '{method}' is not allowed; defaults belong to the trait declaration"
                ),
                *span,
            ),
            TypeError::DefaultInClosure { param, span } => (
                Code::ClosureInvalid,
                format!(
                    "default value for parameter '{param}' is not allowed in closure; closures erase defaults when assigned to a function type"
                ),
                *span,
            ),
            TypeError::ClosureMutBindingRequired { name, span } => (
                Code::ClosureInvalid,
                format!(
                    "cannot call `fn mut` closure '{name}' through a non-`mut` binding; use `let mut {name}`"
                ),
                *span,
            ),
            TypeError::ClosureAtCmBoundary {
                function,
                position,
                span,
            } => (
                Code::CmBoundaryType,
                format!(
                    "closure type in {position} of `{function}` is not allowed: closures cannot cross the Component Model boundary"
                ),
                *span,
            ),
            TypeError::SliceAtCmBoundary {
                function,
                position,
                span,
            } => (
                Code::CmBoundaryType,
                format!(
                    "slice type in {position} of `{function}` is not allowed: a slice is a reference view with no Component Model representation; use `List<T>` or `Array<T>`"
                ),
                *span,
            ),
            TypeError::DefaultInExportFn {
                function,
                param,
                span,
            } => (
                Code::CmBoundaryType,
                format!(
                    "default value for parameter '{param}' in export fn '{function}' is not allowed; the Component Model ABI requires every parameter at the boundary"
                ),
                *span,
            ),
            TypeError::WireDefaultAttr { field, span } => (
                Code::UnsupportedFeature,
                format!(
                    "`#[wire(default)]` is not supported; give field '{field}' a default value instead, e.g. `{field}: T = <value>` (a field with a default value is optional on deserialize)"
                ),
                *span,
            ),
            TypeError::ResumeOutsideHandler { span } => (
                Code::UnsupportedFeature,
                "`resume` is only valid inside an effect handler method body".to_string(),
                *span,
            ),
            TypeError::AsyncUserEffectHandlerUnsupported {
                interface_name,
                op_name,
                span,
            } => (
                Code::UnsupportedFeature,
                format!(
                    "cannot handle async operation '{interface_name}::{op_name}': only async operations backed by a Component Model import (e.g. wasi:http 'Client::send') can be handled"
                ),
                *span,
            ),
            TypeError::AsyncOpMustReturnAsyncCall { op_name, span } => (
                Code::TypeMismatch,
                format!("async operation '{op_name}' must return `AsyncCall<T>`"),
                *span,
            ),
            TypeError::HandlerEffectNotImplemented {
                type_name,
                interface_name,
                span,
            } => (
                Code::EffectHandlerInvalid,
                format!(
                    "handler value of type '{type_name}' does not implement interface '{interface_name}'"
                ),
                *span,
            ),
            TypeError::BundledHandlerImplementsNoEffect { type_name, span } => (
                Code::EffectHandlerInvalid,
                format!(
                    "handler value of type '{type_name}' does not implement any effect; use `with E => h do` instead"
                ),
                *span,
            ),
            TypeError::BundledHandlerUnsupportedHandlerType {
                type_name,
                type_kind,
                span,
            } => (
                Code::UnsupportedFeature,
                format!(
                    "bundled effect handler `with h do` is not supported for handler type '{type_name}' ({type_kind}); use the explicit form `with E => h do` instead"
                ),
                *span,
            ),
            TypeError::NotAnEffect { name, span } => (
                Code::UnknownType,
                format!(
                    "'{}' is not an effect; only effect names are valid in `with E => h do` clauses",
                    unalias_namespace_member(name)
                ),
                *span,
            ),
            TypeError::GenericEffectParamNotInstallable { name, span } => (
                Code::UnsupportedFeature,
                format!(
                    "cannot install a handler for generic effect parameter '{name}'; abstract effect parameters are propagation-only — use a concrete effect name in `with`"
                ),
                *span,
            ),
            TypeError::CompilerItemAttr { message, span } => {
                (Code::CompilerItemAttr, message.clone(), *span)
            }
            TypeError::ResourceExtends { message, span } => {
                (Code::ResourceExtends, message.clone(), *span)
            }
            TypeError::ResourceClasses { message, span } => {
                (Code::ResourceClasses, message.clone(), *span)
            }
            TypeError::AmbiguousResourceMethod {
                method,
                resource,
                trait_name,
                span,
            } => (
                Code::AmbiguousCandidate,
                format!(
                    "ambiguous method '{method}': declared by resource '{resource}' and by trait '{trait_name}'; name one, e.g. '{resource}::{method}(&value)' or '{trait_name}::{method}(&value)'"
                ),
                *span,
            ),
            TypeError::BareGenericFunctionRef { name, span } => (
                Code::GenericFunctionRef,
                {
                    let name = unalias_namespace_member(name);
                    format!(
                        "cannot reference generic function '{name}' bare; supply type arguments via turbofish (e.g., `{name}::<…>`) or wrap in a closure (e.g., `|x| {name}(x)`)"
                    )
                },
                *span,
            ),
            TypeError::GenericFunctionRefArgCountMismatch {
                name,
                expected,
                found,
                span,
            } => (
                Code::GenericFunctionRef,
                format!(
                    "wrong number of type arguments for generic function '{}': expected {expected}, found {found}",
                    unalias_namespace_member(name)
                ),
                *span,
            ),
            TypeError::GenericFunctionRefArityMismatch {
                name,
                expected_params,
                found_params,
                span,
            } => (
                Code::GenericFunctionRef,
                {
                    let name = unalias_namespace_member(name);
                    format!(
                        "cannot reference generic function '{name}' here: expected {expected_params} parameter{}, but '{name}' takes {found_params}",
                        if *expected_params == 1 { "" } else { "s" },
                    )
                },
                *span,
            ),
        }
    }
}

impl From<TypeError> for Diagnostic {
    fn from(e: TypeError) -> Self {
        use crate::compiler_host::{DiagnosticSpan, Severity};
        let (code, message, span) = e.render();
        Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span: Some(DiagnosticSpan::from_span(&span, None)),
        }
    }
}

/// How the frame enclosing a closure reaches one binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OuterReach {
    /// A local of the enclosing frame, at this index.
    ParentLocal(u32),
    /// A slot of the enclosing frame's own environment, which frame registers
    /// once this closure's body has been walked, so no index is known here.
    ParentEnv,
}

/// One binding an enclosing frame can reach. `local` describes the binding;
/// `reach` says where the enclosing frame finds it.
#[derive(Debug, Clone)]
pub(super) struct OuterBinding {
    pub(super) local: LocalVar,
    pub(super) reach: OuterReach,
}

/// One environment slot of a closure under construction.
#[derive(Debug, Clone)]
pub(super) struct CaptureSlot {
    pub(super) index: u32,
    pub(super) reach: OuterReach,
}

/// Local variable information during resolution. The name is the key of the
/// scope map holding it, so it is not repeated here.
#[derive(Debug, Clone)]
pub(super) struct LocalVar {
    pub(super) type_id: TypeId,
    pub(super) index: u32,

    pub(super) is_mut: bool,
    /// `AstId` of the node that introduced this binding (pattern, parameter,
    /// closure parameter). `None` for elaborator-synthesized temporaries whose
    /// names cannot be referenced from source (e.g., `$qm_v`, `$b`).
    ///
    /// Used by [`Elaborator`] to record `use → def` edges when an `IdentExpr`
    /// resolves to this local, so that LSP can translate a cursor position
    /// into the defining symbol's `AstId`.
    pub(super) defining_ast_id: Option<AstId>,
}

/// Method lookup result including return type, self parameter kind, and parameter types
/// Which type's `impl` block answers a method call.
///
/// A newtype inherits its base's methods, and the two cases are named
/// differently — an inherited method is named after the type that declares it.
/// Spelling that as `Option<TypeId>` left "`None` means the receiver's own
/// impl" as a convention a reader had to know, and the question "is this the
/// receiver's own?" got asked downstream by comparing names instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MethodOwner {
    /// The receiver's own `impl` block.
    Receiver,
    /// Inherited through the receiver's newtype chain from the type that
    /// declares it — the innermost such type for a chained newtype.
    InheritedFrom(TypeId),
    /// Inherited through the receiver's `extends` chain from the resource that
    /// declares it. `Self` stays that resource, so nothing re-types to the
    /// receiver ([Resource Inheritance](../../docs/wep-2026-04-28-resource-inheritance.md)).
    Ancestor(TypeId),
}

impl MethodOwner {
    /// The type whose `impl` declares the method: the receiver itself unless
    /// the method was inherited.
    pub(super) fn declaring(self, receiver: TypeId) -> TypeId {
        match self {
            Self::Receiver => receiver,
            Self::InheritedFrom(owner) | Self::Ancestor(owner) => owner,
        }
    }

    /// The type it was inherited from, or `None` when the receiver owns it.
    pub(super) fn inherited(self) -> Option<TypeId> {
        match self {
            Self::Receiver => None,
            Self::InheritedFrom(owner) | Self::Ancestor(owner) => Some(owner),
        }
    }

    /// The newtype base whose signature re-types to the receiver — `Point::add`
    /// called on a `Location` takes and returns `Location`. An `extends`
    /// ancestor does not: its `Self` is fixed at the declaring resource.
    pub(super) fn newtype_base(self) -> Option<TypeId> {
        match self {
            Self::Receiver | Self::Ancestor(_) => None,
            Self::InheritedFrom(owner) => Some(owner),
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct MethodInfo {
    /// The method this lookup selected. A call's use→def edge is recorded from
    /// here, so it names the impl dispatch chose. `None` where no declaration
    /// backs the signature: tuple builtins, auto-derived `Eq` / `Ord`, the
    /// error-recovery placeholder.
    pub(super) method_def: Option<DefId>,
    pub(super) return_type: TypeId,
    pub(super) self_kind: ast::SelfKind,
    /// Parameter types (excluding self)
    pub(super) param_types: Vec<TypeId>,
    /// Whether each parameter (excluding self) is declared `mut`
    pub(super) param_is_mut: Vec<bool>,
    /// Declared default-value expressions for each parameter (excluding self),
    /// parallel to `param_types`. Populated from the method's AST; empty vec
    /// means "no defaults known" (lookups that don't bother populating this).
    pub(super) param_defaults: Vec<Option<ast::Expr>>,
    /// Parameter names (excluding self), parallel to `param_types`. Used when
    /// expanding defaults that reference earlier parameters (e.g. `fn f(w, h = w)`).
    /// Empty vec when `param_defaults` is also empty.
    pub(super) param_names: Vec<String>,
    /// Which type's `impl` answers this call. Naming and signature
    /// substitution both read the declaring type from here rather than
    /// re-deriving it from the receiver.
    pub(super) owner: MethodOwner,
    /// CM canonical name from `#[cm("...")]` on resource methods.
    pub(super) cm_name: Option<String>,
    /// True when the method was found on a reference type impl (e.g., `impl Trait for &T`).
    /// The receiver needs an additional auto-ref for `&self` methods (Self is &T, so &self is &&T).
    pub(super) is_ref_impl: bool,
    /// The method's own slots, in declaration order, as the signature dispatch
    /// selected holds them. Empty where the method declares none.
    pub(super) method_type_param_ids: Vec<TypeId>,
    /// The same slots as the declaration wrote them, parallel to
    /// [`Self::method_type_param_ids`] — the bounds to enforce and the
    /// defaults to fill, which only AST carries.
    pub(super) method_own_params: Vec<ast::GenericParam>,
    /// The module the matched `impl` block lives in. For inherent methods this
    /// is where the method body is registered, which is NOT always the receiver
    /// type's defining module (e.g. a user-written `impl List<u8>` on the
    /// prelude `List`). The call site uses this for the body's `module_source`
    /// so cross-module inherent impls resolve. `None` when the producer did not
    /// determine it (callers fall back to the receiver type's module).
    pub(super) impl_module: Option<ModuleSource>,
    /// True when the matched impl is on a concrete generic instantiation
    /// (`impl List<u8>` / `impl Tag for List<u8>`). Such a method is a concrete
    /// function named per-instantiation (`List<u8>::method`) and called
    /// directly, so the call site emits no `monomorph_info` for it.
    pub(super) from_concrete_impl: bool,
    /// True when the receiver `self` is taken by value (not `&self` / `&mut
    /// self` / `self: &T`), so a call transfers ownership of the receiver.
    /// Mirrors `resource_cleanup`'s `owned_self` at the semantic layer; the
    /// move check reads it to flag use-after-move through a consuming method.
    pub(super) consumes_self: bool,
    /// An inherent member's declared rung. `None` where the member does not
    /// decide its own reach: trait impls, resource methods, builtins.
    pub(super) inherent_visibility: Option<Visibility>,
    /// Where [`Self::param_defaults`] were written, when that is not the
    /// selected method's own module: the trait it implements declares them
    /// (WEP 2026-04-11), and a default resolves in the scope that wrote it.
    pub(super) defaults_module: Option<ModuleSource>,
    /// The matched `impl` block's type parameters, standing for the receiver's
    /// type arguments. A default naming one (`v: T = T::default()`) resolves
    /// against them. Empty where the block declares none, or where the lookup
    /// answers from no block at all.
    pub(super) impl_type_bindings: Vec<DefaultTypeBinding>,
}

impl MethodInfo {
    /// A `&self` method taking nothing that no declaration backs: a tuple
    /// builtin, or the placeholder a failed lookup continues with.
    pub(super) fn undeclared(return_type: TypeId) -> Self {
        Self {
            method_def: None,
            return_type,
            self_kind: ast::SelfKind::Ref,
            param_types: vec![],
            param_is_mut: vec![],
            param_defaults: vec![],
            param_names: vec![],
            owner: MethodOwner::Receiver,
            cm_name: None,
            is_ref_impl: false,
            method_type_param_ids: vec![],
            method_own_params: vec![],
            impl_module: None,
            from_concrete_impl: false,
            consumes_self: false,
            inherent_visibility: None,
            defaults_module: None,
            impl_type_bindings: Vec::new(),
        }
    }
}

/// Labeled block expression target for tracking break types
#[derive(Debug, Clone)]
pub(super) struct LabeledBlockTarget {
    /// The label name
    pub(super) label: String,
    /// Types collected from `break label: expr;` statements
    pub(super) break_types: Vec<TypeId>,
    /// Expected type propagated from the labeled block's use site, if any.
    /// `break label: expr` values are resolved against this so a literal
    /// (e.g. `break label: 10` with `let x: i64 = ...`) coerces correctly.
    pub(super) expected_type: Option<TypeId>,
}

/// A position whose pattern must match every value it is given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BindingSite {
    Let,
    ForOf,
}

/// Function context during resolution with scope tracking
pub(super) struct FunctionContext {
    /// Stack of scopes (each scope maps name -> `LocalVar`)
    pub(super) scopes: Vec<IndexMap<String, LocalVar>>,
    /// Next local index (Wasm locals are function-wide). Private: a uniquifier
    /// comes from [`FunctionContext::fresh_serial`], a count from
    /// [`FunctionContext::local_count`].
    next_local: u32,
    /// Return type of the function (unit for async fns, since they don't Wasm-return a value)
    pub(super) return_type: TypeId,
    /// Whether this is an async function (`export async fn`).
    /// In async fns, `return expr` is forbidden; use `task return expr` instead.
    pub(super) is_async: bool,
    /// The type that `task return` must accept (= the declared return type annotation).
    /// `Some(type_id)` only for async functions.
    pub(super) task_return_type: Option<TypeId>,
    /// Local-variable metadata (name, type, mutability) in declaration
    /// order. The single source of truth for the function/closure's
    /// local namespace — `add_local` pushes here, and consumers that
    /// need a `Vec<TypeId>` (e.g. `TirFunction::local_types`,
    /// `TirGlobal::local_types`) project `locals.iter().map(|l| l.type_id)`
    /// at the point of emission.
    pub(super) locals: Vec<TirLocal>,
    /// Local indices that have their address taken (&x or &mut x)
    pub(super) address_taken_locals: IndexSet<u32>,
    /// Bindings the enclosing frame can reach, and how it reaches each: its own
    /// locals, plus what it reaches by capture. Only set for closure contexts.
    pub(super) outer_locals: IndexMap<String, OuterBinding>,
    /// Captured bindings detected during resolution (name -> slot). Only used
    /// for closure contexts.
    pub(super) captured_vars: IndexMap<String, CaptureSlot>,
    /// Whether [`Self::seed_captures`] filled `captured_vars`, which closes it:
    /// a frame replaying a recorded environment reads slots, never adds one.
    captures_seeded: bool,
    /// Stack of labeled block expression targets for tracking break types
    pub(super) labeled_block_targets: Vec<LabeledBlockTarget>,
    /// Stack of all active labels (from labeled blocks and labeled block expressions)
    pub(super) active_labels: Vec<String>,
    /// Current function name for `#function` compile-time literal
    pub(super) function_name: String,
    /// Deref overrides for mutable closures: maps original var name -> (ref var name, inner type)
    /// When a variable is in this map, lookups return `*$ref_name` instead of the value.
    pub(super) deref_overrides: IndexMap<String, (String, TypeId)>,
    /// Box types for outer address-taken locals: maps outer var name -> `&mut T` type.
    /// When capturing such a variable, use `DerefCapture` to read through the box.
    pub(super) outer_box_types: IndexMap<String, TypeId>,
    /// Per-local closure parameter defaults for `let f = |...| ...` bindings.
    /// Keyed by local variable name; stores `(param_name, default_expr)` in declaration order.
    pub(super) closure_defaults: IndexMap<String, Vec<(String, Option<Expr>)>>,
    /// True when this context represents the body of a method inside an
    /// `impl Effect for Type` block, i.e. an effect handler operation.
    /// `resume value` is only valid in such contexts.
    pub(super) in_handler_method: bool,
    /// One per-function serial behind every name a desugaring step mints: an
    /// `assert` label, a `for` body, an iterator local, a template's holes. Read
    /// through [`FunctionContext::fresh_serial`].
    next_internal: u32,
    /// The body labels of the enclosing C-style `for`s, innermost last; reify
    /// lowers a naked `continue` to `break <last>` so `update` runs.
    pub(super) for_continue_labels: Vec<String>,
    /// The binding site whose pattern is being resolved, when it must match
    /// every value. `None` inside `match`, `if let` and `while let`.
    pub(super) irrefutable_site: Option<BindingSite>,
    /// Power-assert capture side-channel. `Some` only while
    /// [`Elaborator::desugar_assert`] is resolving an assert condition;
    /// the [`Elaborator::resolve_expr`] entry consults it to extract
    /// scanner-flagged sub-expressions into `let $vK = …;` bindings
    /// as they are resolved. Outside an assert this is always `None`,
    /// so the hook is a single `Option` discriminant check on the hot
    /// path.
    pub(super) assert_capture_ctx: Option<AssertCaptureContext>,
    /// Reify-side counterpart to [`Self::assert_capture_ctx`].
    /// Independent so production and reify never share state.
    pub(super) reify_assert_capture_ctx: Option<ReifyAssertCaptureContext>,
    /// Annotate side-channel: a compound-assign sub-piece's `AstId` → its
    /// resolved type. [`Elaborator::resolve_expr`] returns this instead of
    /// re-resolving the node, so it is walked once (matching reify's
    /// `compound_overrides`). Empty outside `resolve_compound_assign`.
    pub(super) compound_hoist_types: IndexMap<AstId, TypeId>,
    /// Subscripts an assignment target projects through (`h[0].n = v`). They
    /// name a `&mut` place rather than a value read, so they take `IndexRefMut`
    /// like `&mut h[0]` does. Empty outside an assignment target.
    pub(super) mut_place_subscripts: IndexSet<AstId>,
    /// Local slots bound as the index of an enclosing variadic `for let [i, v]
    /// of t.enumerate()`. Such a binding is a compile-time constant once the
    /// loop is unrolled, so it is the one non-literal a pack-typed tuple
    /// accepts as a subscript (`slots[i]`).
    ///
    /// Keyed by slot, not by name: a shadowing `let i = …` inside the body is a
    /// different slot and an ordinary run-time value, which the unroller could
    /// not fold. Empty outside a variadic enumerate body.
    pub(super) variadic_enumerate_indices: Vec<u32>,
}

/// A stretch of the walk with one `FunctionContext` field replaced. Derefs to
/// the context, and puts the enclosing value back on drop.
#[must_use]
pub(super) struct FieldFrame<'c, T: Default> {
    ctx: &'c mut FunctionContext,
    field: fn(&mut FunctionContext) -> &mut T,
    enclosing: T,
}

impl<T: Default> Deref for FieldFrame<'_, T> {
    type Target = FunctionContext;
    fn deref(&self) -> &FunctionContext {
        self.ctx
    }
}

impl<T: Default> DerefMut for FieldFrame<'_, T> {
    fn deref_mut(&mut self) -> &mut FunctionContext {
        self.ctx
    }
}

impl<T: Default> Drop for FieldFrame<'_, T> {
    fn drop(&mut self) {
        *(self.field)(self.ctx) = std::mem::take(&mut self.enclosing);
    }
}

/// A stretch of the walk with one more entry on a `FunctionContext` stack.
/// Derefs to the context, and pops the entry on drop.
#[must_use]
pub(super) struct StackFrame<'c, T> {
    ctx: &'c mut FunctionContext,
    stack: fn(&mut FunctionContext) -> &mut Vec<T>,
    depth: usize,
}

impl<T> Deref for StackFrame<'_, T> {
    type Target = FunctionContext;
    fn deref(&self) -> &FunctionContext {
        self.ctx
    }
}

impl<T> DerefMut for StackFrame<'_, T> {
    fn deref_mut(&mut self) -> &mut FunctionContext {
        self.ctx
    }
}

impl<T> Drop for StackFrame<'_, T> {
    fn drop(&mut self) {
        let stack = (self.stack)(self.ctx);
        assert_eq!(stack.len(), self.depth, "a frame pops the entry it pushed");
        stack.pop();
    }
}

/// A labeled block's break target and label, both in scope. Derefs to the
/// context, and pops both on drop.
#[must_use]
pub(super) struct LabeledBlockFrame<'c> {
    ctx: &'c mut FunctionContext,
    targets_depth: usize,
    labels_depth: usize,
}

impl LabeledBlockFrame<'_> {
    fn pop(&mut self) -> LabeledBlockTarget {
        assert_eq!(
            self.ctx.active_labels.len(),
            self.labels_depth,
            "a labeled block pops the label it pushed"
        );
        assert_eq!(
            self.ctx.labeled_block_targets.len(),
            self.targets_depth,
            "a labeled block pops the target it pushed"
        );
        self.ctx.active_labels.pop();
        self.ctx
            .labeled_block_targets
            .pop()
            .expect("a labeled block's target is on the stack")
    }

    /// Leave the block, handing back the breaks it collected.
    pub(super) fn finish(self) -> LabeledBlockTarget {
        ManuallyDrop::new(self).pop()
    }
}

impl Deref for LabeledBlockFrame<'_> {
    type Target = FunctionContext;
    fn deref(&self) -> &FunctionContext {
        self.ctx
    }
}

impl DerefMut for LabeledBlockFrame<'_> {
    fn deref_mut(&mut self) -> &mut FunctionContext {
        self.ctx
    }
}

impl Drop for LabeledBlockFrame<'_> {
    fn drop(&mut self) {
        self.pop();
    }
}

impl FunctionContext {
    pub(super) fn replacing<T: Default>(
        &mut self,
        field: fn(&mut FunctionContext) -> &mut T,
        value: T,
    ) -> FieldFrame<'_, T> {
        let enclosing = std::mem::replace(field(self), value);
        FieldFrame {
            ctx: self,
            field,
            enclosing,
        }
    }

    pub(super) fn pushing<T>(
        &mut self,
        stack: fn(&mut FunctionContext) -> &mut Vec<T>,
        entry: T,
    ) -> StackFrame<'_, T> {
        stack(self).push(entry);
        let depth = stack(self).len();
        StackFrame {
            ctx: self,
            stack,
            depth,
        }
    }

    /// A lexical block, whose bindings go out of scope with the frame.
    pub(super) fn enter_scope(&mut self) -> StackFrame<'_, IndexMap<String, LocalVar>> {
        self.pushing(|ctx| &mut ctx.scopes, IndexMap::default())
    }

    /// A stretch where `break label` names a known target.
    pub(super) fn enter_label(&mut self, label: String) -> StackFrame<'_, String> {
        self.pushing(|ctx| &mut ctx.active_labels, label)
    }

    /// A loop's body, where a naked `continue` targets this loop and not an
    /// enclosing C-style `for`.
    pub(super) fn enter_loop(&mut self) -> FieldFrame<'_, Vec<String>> {
        self.replacing(|ctx| &mut ctx.for_continue_labels, Vec::new())
    }

    /// A stretch where the field `field` selects also holds `items`.
    fn extending<T: Default + Clone + Extend<I>, I>(
        &mut self,
        field: fn(&mut FunctionContext) -> &mut T,
        items: impl IntoIterator<Item = I>,
    ) -> FieldFrame<'_, T> {
        let mut extended = field(self).clone();
        extended.extend(items);
        self.replacing(field, extended)
    }

    /// A stretch where `subscripts` name `&mut` places rather than values read.
    pub(super) fn marking_mut_places(
        &mut self,
        subscripts: &[AstId],
    ) -> FieldFrame<'_, IndexSet<AstId>> {
        self.extending(
            |ctx| &mut ctx.mut_place_subscripts,
            subscripts.iter().copied(),
        )
    }

    /// A variadic loop's body, where `index` is a constant subscript.
    pub(super) fn enter_enumerate_body(&mut self, index: Option<u32>) -> FieldFrame<'_, Vec<u32>> {
        self.extending(|ctx| &mut ctx.variadic_enumerate_indices, index)
    }

    /// Enter a labeled block in either position, so a `break LABEL` inside
    /// resolves to the innermost block of that name.
    pub(super) fn enter_labeled_block(
        &mut self,
        label: String,
        expected_type: Option<TypeId>,
    ) -> LabeledBlockFrame<'_> {
        self.labeled_block_targets.push(LabeledBlockTarget {
            label: label.clone(),
            break_types: Vec::new(),
            expected_type,
        });
        self.active_labels.push(label);
        LabeledBlockFrame {
            targets_depth: self.labeled_block_targets.len(),
            labels_depth: self.active_labels.len(),
            ctx: self,
        }
    }

    pub(super) fn new(return_type: TypeId, function_name: String) -> Self {
        Self {
            scopes: vec![IndexMap::default()], // Start with one scope for function parameters
            next_local: 0,
            return_type,
            is_async: false,
            task_return_type: None,
            locals: Vec::new(),
            address_taken_locals: IndexSet::default(),
            outer_locals: IndexMap::default(),
            captured_vars: IndexMap::default(),
            captures_seeded: false,
            labeled_block_targets: Vec::new(),
            active_labels: Vec::new(),
            function_name,
            deref_overrides: IndexMap::default(),
            outer_box_types: IndexMap::default(),
            closure_defaults: IndexMap::default(),
            in_handler_method: false,
            next_internal: 0,
            for_continue_labels: Vec::new(),
            irrefutable_site: None,
            assert_capture_ctx: None,
            reify_assert_capture_ctx: None,
            compound_hoist_types: IndexMap::default(),
            mut_place_subscripts: IndexSet::default(),
            variadic_enumerate_indices: Vec::new(),
        }
    }

    /// Create a closure context with outer scope access for capture detection.
    ///
    /// `type_table` is used to compute `&mut T` types for address-taken outer locals,
    /// enabling correct `DerefCapture` behaviour when those locals are captured.
    pub(super) fn new_closure(
        return_type: TypeId,
        outer_ctx: &FunctionContext,
        type_table: &RefCell<TypeTable>,
    ) -> Self {
        // Everything the parent can reach, not just what it owns. Its own
        // captures go in first, so a parent local of that name shadows one.
        let mut outer_locals: IndexMap<String, OuterBinding> = IndexMap::default();
        for (name, binding) in &outer_ctx.outer_locals {
            outer_locals.insert(
                name.clone(),
                OuterBinding {
                    local: binding.local.clone(),
                    reach: OuterReach::ParentEnv,
                },
            );
        }
        for scope in &outer_ctx.scopes {
            for (name, local) in scope {
                outer_locals.insert(
                    name.clone(),
                    OuterBinding {
                        local: local.clone(),
                        reach: OuterReach::ParentLocal(local.index),
                    },
                );
            }
        }

        // Box types, asked of the binding each name resolves to: read off
        // `outer_locals`, a parent local shadowing a boxed one is not boxed.
        let mut outer_box_types = IndexMap::default();
        for (name, binding) in &outer_locals {
            let ref_type = match binding.reach {
                OuterReach::ParentLocal(index) => outer_ctx
                    .address_taken_locals
                    .contains(&index)
                    .then(|| type_table.borrow_mut().make_mut_ref(binding.local.type_id)),
                // Boxed where it is owned, however many frames out that is.
                OuterReach::ParentEnv => outer_ctx.outer_box_types.get(name).copied(),
            };
            if let Some(ref_type) = ref_type {
                outer_box_types.insert(name.clone(), ref_type);
            }
        }

        let function_name = format!("{}::{{closure}}", outer_ctx.function_name);

        Self {
            scopes: vec![IndexMap::default()],
            next_local: 0,
            return_type,
            is_async: false, // Closures are never async
            task_return_type: None,
            locals: Vec::new(),
            address_taken_locals: IndexSet::default(),
            outer_locals,
            captured_vars: IndexMap::default(),
            captures_seeded: false,
            labeled_block_targets: Vec::new(),
            active_labels: Vec::new(),
            function_name,
            deref_overrides: IndexMap::default(),
            outer_box_types,
            closure_defaults: IndexMap::default(),
            // Closures inside a handler method body are NOT themselves
            // handler methods — `resume` returns from the enclosing
            // operation, not from the closure.
            in_handler_method: false,
            next_internal: 0,
            for_continue_labels: Vec::new(),
            irrefutable_site: None,
            assert_capture_ctx: None,
            reify_assert_capture_ctx: None,
            compound_hoist_types: IndexMap::default(),
            mut_place_subscripts: IndexSet::default(),
            variadic_enumerate_indices: Vec::new(),
        }
    }

    /// A serial no other desugaring step in this function holds, for the names
    /// one step mints. It advances on read, so a step that takes its serial
    /// before recursing keeps it: a template nested in a hole, or a `for-of`
    /// over a `for-of`, mints names of its own (issue #1987).
    pub(super) fn fresh_serial(&mut self) -> u32 {
        let serial = self.next_internal;
        self.next_internal += 1;
        serial
    }

    /// How many locals the function has allocated.
    pub(super) fn local_count(&self) -> u32 {
        self.next_local
    }

    /// Add a local variable to the current scope.
    ///
    /// `defining_ast_id` identifies the source AST node that introduced this
    /// binding and is used by the elaborator to record `use → def` edges. Pass
    /// `Some(id)` for user-visible bindings (let patterns, parameters, closure
    /// parameters); pass `None` for elaborator-synthesized temporaries whose
    /// names cannot appear in source (e.g., `$qm_v`, `$b`).
    pub(super) fn add_local(
        &mut self,
        name: String,
        type_id: TypeId,
        is_mut: bool,
        defining_ast_id: Option<AstId>,
    ) -> u32 {
        debug_assert!(
            defining_ast_id.is_none(),
            "a source-level binding must carry its span: use `add_local_at`"
        );
        self.add_local_at(name, type_id, is_mut, defining_ast_id, Span::default())
    }

    /// [`Self::add_local`] for a binding written in source, whose span a
    /// diagnostic can point at.
    pub(super) fn add_local_at(
        &mut self,
        name: String,
        type_id: TypeId,
        is_mut: bool,
        defining_ast_id: Option<AstId>,
        span: Span,
    ) -> u32 {
        let index = self.next_local;
        self.next_local += 1;
        self.locals.push(TirLocal {
            name: name.clone(),
            type_id,
            is_mut,
            span,
        });

        let scope = self.scopes.last_mut().unwrap();
        scope.insert(
            name,
            LocalVar {
                type_id,
                index,
                is_mut,
                defining_ast_id,
            },
        );
        index
    }

    /// Reach local `index` as `name` from the current scope too, the way a
    /// minted local stands for a binding source can spell.
    pub(super) fn name_local(&mut self, name: String, index: u32) {
        let TirLocal {
            type_id, is_mut, ..
        } = self.locals[index as usize];
        let scope = self.scopes.last_mut().unwrap();
        scope.insert(
            name,
            LocalVar {
                type_id,
                index,
                is_mut,
                defining_ast_id: None,
            },
        );
    }

    /// Look up a variable by name (searches from innermost to outermost scope)
    pub(super) fn lookup(&self, name: &str) -> Option<&LocalVar> {
        for scope in self.scopes.iter().rev() {
            if let Some(local) = scope.get(name) {
                return Some(local);
            }
        }
        None
    }

    /// Whether `expr` names the index of an enclosing variadic `.enumerate()`,
    /// the one non-literal subscript a pack-typed tuple admits.
    pub(super) fn is_variadic_enumerate_index(&self, expr: &ast::Expr) -> bool {
        matches!(expr, ast::Expr::Ident(ident)
            if self.lookup(&ident.name)
                .is_some_and(|local| self.variadic_enumerate_indices.contains(&local.index)))
    }

    /// The binding `name` names here: one this frame owns, or one it reaches
    /// through its own environment. Registers no capture, unlike
    /// [`Self::lookup_or_capture`].
    pub(super) fn binding(&self, name: &str) -> Option<&LocalVar> {
        self.lookup(name)
            .or_else(|| self.outer_locals.get(name).map(|outer| &outer.local))
    }

    /// Run `body` with the surrounding frame's bindings out of scope, keeping
    /// local allocation on this context — an inlined constant body resolves
    /// names in its own module, but its locals live in the frame it lands in.
    pub(super) fn with_caller_bindings_hidden<R>(
        &mut self,
        body: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let mut scopes = self.replacing(|ctx| &mut ctx.scopes, vec![IndexMap::default()]);
        let mut outer = scopes.replacing(|ctx| &mut ctx.outer_locals, IndexMap::default());
        let mut derefs = outer.replacing(|ctx| &mut ctx.deref_overrides, IndexMap::default());
        let mut boxes = derefs.replacing(|ctx| &mut ctx.outer_box_types, IndexMap::default());
        body(&mut boxes)
    }

    /// Look up a variable, checking outer context for captures if in a closure.
    /// Returns either a local variable reference or a capture reference.
    pub(super) fn lookup_or_capture(&mut self, name: &str) -> Option<VarRef> {
        // First check local scopes
        for scope in self.scopes.iter().rev() {
            if let Some(local) = scope.get(name) {
                return Some(VarRef::Local {
                    index: local.index,
                    type_id: local.type_id,
                    defining_ast_id: local.defining_ast_id,
                });
            }
        }

        // Check deref overrides (for mutable closures: `count` -> `*$ref_count`)
        if let Some((ref_name, inner_type_id)) = self.deref_overrides.get(name).cloned()
            && let Some(ref_binding) = self.outer_locals.get(&ref_name).cloned()
        {
            let outer_defining_ast_id = self
                .outer_locals
                .get(name)
                .and_then(|b| b.local.defining_ast_id);
            return Some(VarRef::DerefCapture {
                index: self.capture_slot(&ref_name, ref_binding.reach),
                ref_type_id: ref_binding.local.type_id,
                inner_type_id,
                defining_ast_id: outer_defining_ast_id,
            });
        }

        // Check outer context (for closures)
        if let Some(outer) = self.outer_locals.get(name).cloned() {
            let inner_type_id = outer.local.type_id;
            let outer_defining_ast_id = outer.local.defining_ast_id;
            let index = self.capture_slot(name, outer.reach);

            // If this outer local has been address-taken (boxed), capture via DerefCapture
            if let Some(&ref_type_id) = self.outer_box_types.get(name) {
                return Some(VarRef::DerefCapture {
                    index,
                    ref_type_id,
                    inner_type_id,
                    defining_ast_id: outer_defining_ast_id,
                });
            }

            return Some(VarRef::Capture {
                index,
                type_id: inner_type_id,
                defining_ast_id: outer_defining_ast_id,
            });
        }

        None
    }

    /// This closure's environment slot for `name`, allocating one on first use.
    fn capture_slot(&mut self, name: &str, reach: OuterReach) -> u32 {
        if let Some(slot) = self.captured_vars.get(name) {
            return slot.index;
        }
        assert!(
            !self.captures_seeded,
            "in {}: `{name}` reaches this closure's environment, and the record being replayed has no slot for it",
            self.function_name
        );
        let index = self.captured_vars.len() as u32;
        self.captured_vars
            .insert(name.to_string(), CaptureSlot { index, reach });
        index
    }

    /// Open this frame's environment with the slots annotate settled on, in its
    /// order, so the body walk reads slots rather than deciding them again.
    pub(super) fn seed_captures<'n>(&mut self, names: impl IntoIterator<Item = &'n str>) {
        assert!(
            self.captured_vars.is_empty(),
            "in {}: the environment is seeded before the body walk, which is what fills it otherwise",
            self.function_name
        );
        for name in names {
            let Some(binding) = self.outer_locals.get(name) else {
                unreachable!(
                    "in {}: annotate captured `{name}`, which this frame cannot reach",
                    self.function_name
                )
            };
            let reach = binding.reach;
            self.capture_slot(name, reach);
        }
        self.captures_seeded = true;
    }

    /// The captures in slot order, for building `TirCapture` entries. An
    /// address-taken outer local carries its box type (`&mut T`).
    pub(super) fn get_captures(&self) -> Vec<(String, LocalVar, OuterReach)> {
        self.captured_vars
            .iter()
            .enumerate()
            .map(|(position, (name, slot))| {
                assert_eq!(
                    slot.index as usize, position,
                    "in {}: a slot is allocated at the end, so insertion order is slot order",
                    self.function_name
                );
                let Some(outer) = self.outer_locals.get(name) else {
                    unreachable!(
                        "in {}: `{name}` holds a slot, but the enclosing frame cannot reach it",
                        self.function_name
                    )
                };
                let type_id = self
                    .outer_box_types
                    .get(name)
                    .copied()
                    .unwrap_or(outer.local.type_id);
                let local = LocalVar {
                    type_id,
                    ..outer.local
                };
                (name.clone(), local, slot.reach)
            })
            .collect()
    }
}

/// Reference to a variable (either local or captured)
pub(super) enum VarRef {
    Local {
        index: u32,
        type_id: TypeId,
        defining_ast_id: Option<AstId>,
    },
    Capture {
        index: u32,
        type_id: TypeId,
        defining_ast_id: Option<AstId>,
    },
    /// Captured by mutable reference: `*self.$capture_N` (dereferenced)
    DerefCapture {
        index: u32,
        ref_type_id: TypeId,
        inner_type_id: TypeId,
        defining_ast_id: Option<AstId>,
    },
}

impl VarRef {
    /// The type of the value read, which for a `DerefCapture` is what the
    /// captured reference points at rather than the reference.
    pub(super) fn value_type(&self) -> TypeId {
        match *self {
            VarRef::Local { type_id, .. } | VarRef::Capture { type_id, .. } => type_id,
            VarRef::DerefCapture { inner_type_id, .. } => inner_type_id,
        }
    }

    /// The node that introduced the binding, for the use→def edge.
    pub(super) fn defining_ast_id(&self) -> Option<AstId> {
        match *self {
            VarRef::Local {
                defining_ast_id, ..
            }
            | VarRef::Capture {
                defining_ast_id, ..
            }
            | VarRef::DerefCapture {
                defining_ast_id, ..
            } => defining_ast_id,
        }
    }
}

/// The trait a qualified call names, resolved to its identity: the
/// declaration key discriminates same-named traits from different modules,
/// and `args` (present when a turbofish pinned an argument list) are the
/// resolved types, so aliased spellings compare equal. `display` keeps the
/// caller's spelling for diagnostics.
pub(super) struct RequiredTrait {
    pub(super) decl: Resolution,
    pub(super) args: Option<Vec<TypeId>>,
    pub(super) display: String,
}

/// Result of finding a trait method for a type via `find_trait_method_for_type`.
pub(super) struct TraitMethodMatch {
    /// The matched trait as a mangled method name embeds it: named by the
    /// module that declares it, carrying the impl header's type arguments.
    pub(super) trait_name: FqTraitName,
    /// The matched trait's declaration key, resolved from the impl's own
    /// module — two same-named traits from different modules stay distinct.
    pub(super) trait_decl: DefId,
    /// The impl's trait type arguments as resolved types (empty for a trait
    /// with none). Two matches agreeing on `trait_decl` but not here are one
    /// trait at different argument lists — an overload set.
    pub(super) trait_args: Vec<TypeId>,
    pub(super) method_info: MethodInfo,
    pub(super) impl_module_source: ModuleSource,
    /// For blanket impl matches (e.g., `impl<I: Iterator> IntoIterator for I`),
    /// this holds the type parameter name (e.g., `"I"`). `None` for normal impls.
    pub(super) blanket_type_param: Option<String>,
    /// [`Self::blanket_type_param`] as a binder named by its block, so two
    /// blankets of one trait are two templates whatever letter each spells.
    pub(super) blanket_binder: Option<FqTypeName>,
    /// The receiver parameter's bounds as source writes them (`T: Limit`) —
    /// what an ambiguity names two blankets by, neither having a name.
    pub(super) blanket_bounds: Option<String>,
    /// The struct name that actually has the trait impl (may differ from the
    /// receiver's struct name when the impl was found through the newtype chain).
    /// Written form — the impl-index key.
    pub(super) impl_struct_name: String,
    /// [`Self::impl_struct_name`] as the receiver form a mangled name embeds,
    /// resolved from the impl's own module so it matches the name the impl's
    /// methods were defined under.
    pub(super) impl_struct_fq: FqTypeName,
    /// True for blanket ref impls like `impl<T: Inspect> Inspect for &T` where
    /// the inner type is a type parameter. False for specific ref impls like
    /// `impl IntoIterator for &List<T>` where the inner type is a concrete generic.
    pub(super) is_blanket_ref_impl: bool,
    /// The pointee a concrete reference impl targets (`impl Trait for &Inner` →
    /// `Inner`). A reference impl buckets by kind, so nothing else names the
    /// link a newtype inherits it through.
    pub(super) ref_impl_target: Option<TypeId>,
}

impl TraitMethodMatch {
    /// The owner a call on `receiver_base` dispatches with: a newtype inherits
    /// a reference impl the way it inherits a value one, so its signature
    /// re-types to the receiver.
    pub(super) fn owner_for(&self, receiver_base: TypeId, types: &TypeTable) -> MethodOwner {
        match self.ref_impl_target {
            Some(link) if types.newtype_chain_reaches(receiver_base, link) => {
                MethodOwner::InheritedFrom(link)
            }
            _ => self.method_info.owner,
        }
    }
}

/// One type-parameter slot as a declaration-level resolver sees it: the name
/// filling it and the bounds that say what `T::Assoc` means.
#[derive(Debug, Clone, Default)]
pub(super) struct ParamSlot {
    pub(super) name: String,
    pub(super) bounds: Vec<ast::TraitBound>,
    pub(super) is_pack: bool,
}

impl From<&ast::GenericParam> for ParamSlot {
    fn from(param: &ast::GenericParam) -> Self {
        Self {
            name: param.name.clone(),
            bounds: param.bounds.clone(),
            is_pack: param.is_pack,
        }
    }
}

/// A declaration's type parameters as the dense type-argument space holds
/// them: a position here is the index an argument fills.
#[derive(Clone, Default)]
pub(crate) struct RealTypeParams(Vec<ast::GenericParam>);

impl RealTypeParams {
    // An effect or `fn`-bound parameter holds no position in that space, so
    // counting one leaves a slot no argument can fill (`register_generic_params`).
    pub(super) fn of(params: &[ast::GenericParam]) -> Self {
        Self(Self::borrowed(params).into_iter().cloned().collect())
    }

    /// The same parameters borrowed, for a caller that only counts them or
    /// looks for the pack among them.
    pub(super) fn borrowed(params: &[ast::GenericParam]) -> Vec<&ast::GenericParam> {
        params
            .iter()
            .filter(|param| param.is_real_type_param())
            .collect()
    }
}

impl Deref for RealTypeParams {
    type Target = [ast::GenericParam];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ParamSlot {
    /// A declaration's real parameters as slots, in declaration order.
    pub(super) fn list(params: &[ast::GenericParam]) -> Vec<Self> {
        RealTypeParams::borrowed(params)
            .into_iter()
            .map(Self::from)
            .collect()
    }

    /// An `impl` head's parameters as slots, [`Self::list`] minus the effect
    /// parameters, which are no type. The target says where each one sits.
    pub(super) fn impl_list(params: &[ast::GenericParam]) -> Vec<Self> {
        params
            .iter()
            .filter(|param| param.fills_impl_slot())
            .map(Self::from)
            .collect()
    }
}

/// Every data declaration's resolved shape, keyed by declaration.
#[derive(Default, Clone)]
pub(crate) struct DataDecls {
    pub(crate) newtypes: IndexMap<DefId, TypeId>,
    pub(crate) generic_newtypes: IndexMap<DefId, GenericNewtypeInfo>,
    pub(crate) struct_fields: IndexMap<DefId, StructFieldInfo>,
    pub(crate) variant_cases: IndexMap<DefId, VariantInfo>,
    pub(crate) enum_cases: IndexMap<DefId, EnumInfo>,
    pub(crate) flags_cases: IndexMap<DefId, FlagsInfo>,
    pub(crate) resource_types: IndexMap<DefId, ResourceInfo>,
}

impl DataDecls {
    /// Declare `decl` as a distinct flags type, which is also a type name.
    pub(super) fn declare_flags(
        &mut self,
        type_table: &RefCell<TypeTable>,
        def: DefId,
        module_source: ModuleSource,
        decl: &ast::FlagsDecl,
    ) {
        let flags_type = type_table.borrow_mut().make_flags(def);
        type_table
            .borrow_mut()
            .register_decl_type(decl.id, flags_type);
        self.newtypes.insert(def, flags_type);
        self.flags_cases
            .insert(def, FlagsInfo::of_decl(flags_type, module_source, decl));
    }

    /// Declare the concrete newtype `decl_id` over `base`, which is also a type name.
    pub(crate) fn declare_newtype(
        &mut self,
        type_table: &RefCell<TypeTable>,
        def: DefId,
        decl_id: AstId,
        base: TypeId,
    ) {
        let newtype_id = type_table.borrow_mut().make_newtype(def, base);
        type_table
            .borrow_mut()
            .register_decl_type(decl_id, newtype_id);
        self.newtypes.insert(def, newtype_id);
    }

    /// Every declaration any table holds. A `flags` type is in two.
    pub(crate) fn declarations(&self) -> impl Iterator<Item = DefId> + '_ {
        self.known_types()
            .chain(self.resource_types.keys().copied())
    }

    /// [`Self::declarations`] less the resources, which impl-block inference
    /// must not read as a known type name (`impl Request { … }`).
    pub(crate) fn known_types(&self) -> impl Iterator<Item = DefId> + '_ {
        let Self {
            newtypes,
            generic_newtypes,
            struct_fields,
            variant_cases,
            enum_cases,
            flags_cases,
            resource_types: _,
        } = self;
        struct_fields
            .keys()
            .chain(variant_cases.keys())
            .chain(enum_cases.keys())
            .chain(flags_cases.keys())
            .chain(newtypes.keys())
            .chain(generic_newtypes.keys())
            .copied()
    }
}

/// Read-only view resolving a type name from a module's perspective without
/// cloning per-module maps. Precedence, highest first: local additions found
/// during resolution, the current module's own definitions, then its imports
/// (with `use { Foo as Bar }` aliasing) — no global scan beyond that (#1416).
/// All fields are borrowed, so a call site constructs one without allocating.
pub(crate) struct TypeLookup<'a> {
    pub(crate) current_module_source: &'a ModuleSource,
    /// What every name in the program resolves to, answered once by the resolve
    /// pass. The registries this view reads are keyed by declaration, so this is
    /// how a written name reaches one.
    pub(crate) resolutions: &'a Resolutions,
    /// Namespace-import aliases (`use ns from "..."`), by which a `ns::Type`
    /// reference canonicalizes to `ns$Type` (`sem::imports::canonical_ns_ref`).
    pub(crate) namespace_imports: &'a IndexMap<String, ModuleSource>,
    pub(crate) program: &'a DataDecls,
    /// This walk's own additions, read ahead of `program`: its local data
    /// declarations, anonymous shapes and function-local items.
    pub(crate) walk: &'a ModuleDecls,
    /// The declaration indexes, for a caller holding a rendered head rather than
    /// its site. No import alias steers them; a name several modules declare misses.
    pub(crate) decls: &'a TraitEnv,
}

impl<'a> TypeLookup<'a> {
    /// Field info for a struct type's own head — the form with nothing left to
    /// resolve, since the head is already an identity or a shape.
    pub(super) fn struct_fields_of_head(&self, head: StructDef) -> Option<&'a StructFieldInfo> {
        match head {
            StructDef::Decl(def) => self.struct_fields_of(def),
            StructDef::Anon(shape) => self.walk.anon_struct_fields.get(&shape),
        }
    }

    /// The cases of the variant a *written* qualifier names — the `Color` of
    /// `Color::Red`, read off the site the resolve walk answered for.
    ///
    /// The spelling answers only where no walk saw the node.
    pub(super) fn variant_cases_at(
        &self,
        site: Option<AstId>,
        name: &str,
    ) -> Option<&'a VariantInfo> {
        self.variant_cases_of(self.declaration_at(site, name)?)
    }

    /// [`Self::variant_cases_at`] for an `enum`.
    pub(super) fn enum_cases_at(&self, site: Option<AstId>, name: &str) -> Option<&'a EnumInfo> {
        self.enum_cases_of(self.declaration_at(site, name)?)
    }

    /// [`Self::variant_cases_at`] for a `flags` type.
    pub(super) fn flags_members_at(
        &self,
        site: Option<AstId>,
        name: &str,
    ) -> Option<&'a FlagsInfo> {
        self.flags_members_of(self.declaration_at(site, name)?)
    }

    /// The newtype (or `flags` type) `name` names here.
    pub(super) fn newtype(&self, name: &str) -> Option<TypeId> {
        self.newtype_of(self.declaration(name)?)
    }

    /// `def`'s own type parameters as types, in declaration order. `None` for a
    /// generic newtype, which keeps none: its base is substituted as AST.
    pub(super) fn declared_type_param_ids(&self, def: DefId) -> Option<&'a [TypeId]> {
        if let Some(info) = self.struct_fields_of(def)
            && !info.type_param_type_ids.is_empty()
        {
            return Some(&info.type_param_type_ids);
        }
        let info = self.variant_cases_of(def)?;
        (!info.type_param_type_ids.is_empty()).then_some(&*info.type_param_type_ids)
    }

    /// `def`'s real type parameters, for the one declaration of the three kinds
    /// that takes any.
    ///
    /// The one source for its bounds, defaults and arity, so a consumer cannot
    /// answer from a projection that dropped a bound, a pack or a default.
    pub(super) fn declared_generic_params(&self, def: DefId) -> Option<&'a [ast::GenericParam]> {
        if let Some(info) = self.struct_fields_of(def)
            && !info.type_params.is_empty()
        {
            return Some(&info.type_params);
        }
        if let Some(info) = self.variant_cases_of(def)
            && !info.type_params.is_empty()
        {
            return Some(&info.type_params);
        }
        let info = self.generic_newtype_of(def)?;
        (!info.type_params.is_empty()).then_some(&*info.type_params)
    }

    /// Whether every parameter `def` declares has a default, which is what
    /// makes its bare name an application of them all.
    pub(super) fn every_type_param_defaults(&self, def: DefId) -> bool {
        self.declared_generic_params(def)
            .is_some_and(|params| params.iter().all(|p| p.default.is_some()))
    }

    /// `args` extended with the declared default of each parameter the site
    /// left out. `None` when nothing was omitted, or an omitted parameter
    /// declares no default — the arity diagnostics answer for that.
    ///
    /// A default may name a parameter to its left (`struct Both<A, B = A>`),
    /// which stands for that parameter's *argument*, not for whatever the use
    /// site happens to call `A`. So each default is substituted against the
    /// arguments already settled before it resolves.
    pub(super) fn type_args_with_defaults(
        &self,
        def: DefId,
        given: usize,
    ) -> Option<(Vec<ParamSlot>, Vec<ast::Type>)> {
        let params = self.declared_generic_params(def)?;
        if given >= params.len() {
            return None;
        }
        if !self.type_param_defaults_terminate(def) {
            return None;
        }
        let defaults: Option<Vec<ast::Type>> = params[given..]
            .iter()
            .map(|param| param.default.clone())
            .collect();
        Some((params.iter().map(ParamSlot::from).collect(), defaults?))
    }

    /// Whether expanding `def`'s declared defaults reaches a fixpoint.
    ///
    /// A default names a type, whose own defaults name types, and so on. Where
    /// that walk revisits a declaration it is already inside, no amount of
    /// expansion settles the arguments.
    pub(super) fn type_param_defaults_terminate(&self, def: DefId) -> bool {
        let mut expanding = hashmap::IndexSet::default();
        let mut settled = hashmap::IndexSet::default();
        !self.defaults_reach_a_cycle(def, &mut expanding, &mut settled)
    }

    fn defaults_reach_a_cycle(
        &self,
        def: DefId,
        expanding: &mut hashmap::IndexSet<DefId>,
        settled: &mut hashmap::IndexSet<DefId>,
    ) -> bool {
        if settled.contains(&def) {
            return false;
        }
        if !expanding.insert(def) {
            return true;
        }
        let cycles = self
            .declared_generic_params(def)
            .into_iter()
            .flatten()
            .filter_map(|param| param.default.as_ref())
            .any(|default| {
                let mut heads = Vec::new();
                collect_type_heads(default, &mut heads);
                heads.into_iter().any(|(site, name)| {
                    self.declaration_at(Some(site), &name)
                        .is_some_and(|target| {
                            self.defaults_reach_a_cycle(target, expanding, settled)
                        })
                })
            });
        expanding.shift_remove(&def);
        if !cycles {
            settled.insert(def);
        }
        cycles
    }

    /// `def`'s entry in the table `pick` names, this walk's own ahead of the
    /// program's. Keyed by declaration, so no spelling is re-resolved.
    fn data_of<T>(
        &self,
        def: DefId,
        pick: impl Fn(&'a DataDecls) -> &'a IndexMap<DefId, T>,
    ) -> Option<&'a T> {
        pick(&self.walk.local)
            .get(&def)
            .or_else(|| pick(self.program).get(&def))
    }

    pub(super) fn struct_fields_of(&self, def: DefId) -> Option<&'a StructFieldInfo> {
        self.data_of(def, |d| &d.struct_fields)
    }

    pub(super) fn variant_cases_of(&self, def: DefId) -> Option<&'a VariantInfo> {
        self.data_of(def, |d| &d.variant_cases)
    }

    pub(super) fn enum_cases_of(&self, def: DefId) -> Option<&'a EnumInfo> {
        self.data_of(def, |d| &d.enum_cases)
    }

    pub(super) fn flags_members_of(&self, def: DefId) -> Option<&'a FlagsInfo> {
        self.data_of(def, |d| &d.flags_cases)
    }

    pub(super) fn resource_type_of(&self, def: DefId) -> Option<&'a ResourceInfo> {
        self.data_of(def, |d| &d.resource_types)
    }

    pub(super) fn generic_newtype_of(&self, def: DefId) -> Option<&'a GenericNewtypeInfo> {
        self.data_of(def, |d| &d.generic_newtypes)
    }

    /// The type the newtype (or `flags` type) `def` declares.
    pub(super) fn newtype_of(&self, def: DefId) -> Option<TypeId> {
        self.data_of(def, |d| &d.newtypes).copied()
    }

    /// Which of `bounds` declares `assoc_name`.
    pub(super) fn bound_declaring_assoc_type(
        &self,
        bounds: &[ast::TraitBound],
        assoc_name: &str,
    ) -> Option<DefId> {
        self.decls
            .bound_declaring_assoc_type(bounds, assoc_name, |bound| {
                self.declaration_at(Some(bound.id), &bound.name)
            })
    }

    /// The declaration a type reference names.
    ///
    /// The site decides: the walk answered for it once, in the module that
    /// wrote it, so an alias, a namespace prefix and a function-local `struct`
    /// all reach their own declaration with no vantage supplied here. A binder
    /// is not a declaration and gets none. The spelling answers only where the
    /// walk left nothing — `None` for a node the elaborator minted, and
    /// `Unresolved` for a name it could not place, where this module's scope
    /// is the same scope and so the same answer.
    pub(super) fn declaration_at(&self, site: Option<AstId>, name: &str) -> Option<DefId> {
        match site.and_then(|site| self.resolutions.walked(site)) {
            Some(Resolution::Def(def)) => Some(def),
            // Neither is a declaration, and a projection's bare member name
            // would reach whatever else this module calls that.
            Some(Resolution::Binder(_) | Resolution::Projection(_)) => None,
            Some(Resolution::Unresolved) | None => self.declaration(name),
        }
    }

    /// Which declaration `name` names in the frame this view stands in — for a
    /// caller holding a rendering. Not a scope: a name with a site goes through
    /// [`Self::declaration_at`], which reads what the resolve pass recorded.
    ///
    /// The function-local items tried ahead of the indexes are the walk's own
    /// position; a local item is visible only after its declaration statement.
    pub(super) fn declaration(&self, name: &str) -> Option<DefId> {
        let canon = canonical_ns_ref(self.namespace_imports, name);
        let name = canon.as_deref().unwrap_or(name);
        if let Some(def) = self.walk.fn_local_items.get(name) {
            return Some(*def);
        }
        // The frame derivation. A *written* reference reaches this view through
        // `declaration_at`, which asks the site the walk answered for; what is
        // left here arrived holding a rendered head, for which only the
        // declaration index can answer. The three tiers are the module's own
        // reach — what it imported, what it declares, what the prelude gives
        // it — so a declaration this module cannot see stays unseen here.
        self.resolutions
            .imported_as(self.current_module_source, name)
            .or_else(|| {
                self.decls
                    .decls_named(name)
                    .find(|def| self.resolutions.defs().module(*def) == self.current_module_source)
            })
            .or_else(|| self.resolutions.prelude_decl(name))
    }
}

/// A matched `IndexRef` / `IndexRefMut` / `IndexValue` / `IndexAssign` impl.
/// One shape for all four: they differ only in the trait probed and the method
/// name it declares, both of which the caller named to find this.
pub(super) struct IndexingTraitInfo {
    /// The method the matched block declares — two blocks on one type may each
    /// declare it, so a use site records this rather than the name.
    pub(super) method_def: DefId,
    /// The `Output` associated type — for `IndexAssign`, the assigned value's
    /// type.
    pub(super) output_type: TypeId,
    /// Self kind of the trait's method.
    pub(super) self_kind: ast::SelfKind,
    /// The implemented trait, named by the module that declares it.
    pub(super) trait_name: FqTraitName,
    /// Module where the impl block is defined
    pub(super) impl_module_source: ModuleSource,
    /// The trait's index (key) type argument (e.g. `List<i32>`), for subscript
    /// coercion.
    pub(super) index_type: Option<TypeId>,
    /// How to spell the receiver in the dispatched method's name — the base for
    /// a block with parameters, the instantiation for one written at a type.
    pub(super) receiver: FqTypeName,
}

/// Info about an operator trait implementation
#[derive(Clone)]
pub(super) struct ArithmeticTraitInfo {
    /// The `impl` block that matched. The module a dispatch is recorded
    /// against is read off it, so no rendering is compared to find one.
    pub(super) impl_def: DefId,
    /// The Output associated type
    pub(super) output_type: TypeId,
    /// Self kind for the method (&self)
    pub(super) self_kind: ast::SelfKind,
    /// The implemented trait, named by the module that declares it.
    pub(super) trait_name: FqTraitName,
    /// The resolved type of the rhs parameter (first non-self parameter)
    pub(super) rhs_type: Option<TypeId>,
    /// Module that wrote the impl block — where the method body is registered.
    pub(super) impl_module_source: ModuleSource,
}

/// A trait method an operator dispatches to, Self-substituted;
/// [`Elaborator::dispatch_trait_op_method`] builds the call.
pub(super) struct ResolvedTraitMethod {
    /// The declaration dispatch selected — an `impl` block's method, or the
    /// *trait's* where the receiver is a type parameter and only
    /// monomorphization can say which block answers.
    pub(super) method_def: Option<DefId>,
    /// The implemented trait, named by the module that declares it.
    pub(super) trait_name: FqTraitName,
    /// Method name (e.g., "eq", "cmp", "add", "shl", "neg", "bitnot").
    pub(super) method_name: String,
    /// The `impl` block dispatch matched. `None` where none is named: an
    /// auto-derived `Eq` / `Ord`, and a method reached through a type
    /// parameter's bound, whose block monomorphization picks.
    pub(super) impl_def: Option<DefId>,
    /// Written name of the type whose impl matched — the impl-index key. For
    /// newtypes this may be the ultimate base-type name when dispatch falls
    /// back to the base impl.
    pub(super) impl_name: String,
    /// That type's `TypeId`, from which the receiver's fq name is read.
    /// `None` when the receiver is a type parameter, which names no
    /// declaration.
    pub(super) impl_type_id: Option<TypeId>,
    /// `self_kind` from the method signature (almost always `Ref`).
    pub(super) self_kind: ast::SelfKind,
    /// Return type of the method, with `Self` and impl type params
    /// substituted to the concrete receiver instance.
    pub(super) return_type: TypeId,
    /// Expected parameter types (excluding `self`), already substituted.
    /// Length is 0 for unary operator traits (`Neg`, `BitNot`), 1 for
    /// binary operator traits (`Eq`, `Ord`, `Add`, `Shl`, …).  For
    /// `Eq::eq` / `Ord::cmp` this is `[&Self]` substituted to `[&Receiver]`.
    pub(super) param_types: Vec<TypeId>,
    /// True when the receiver is a type parameter (`T: Ord`) rather than a
    /// concrete type. Propagated into `LocalMethodName::is_type_param_receiver`
    /// so monomorphization substitutes it correctly.
    pub(super) is_type_param_receiver: bool,
}

impl ResolvedTraitMethod {
    /// A method a type parameter's bound supplies; which impl answers is
    /// monomorphization's to say.
    pub(super) fn through_bound(
        param: &str,
        trait_name: FqTraitName,
        method_name: &str,
        info: MethodInfo,
    ) -> Self {
        Self {
            method_def: info.method_def,
            trait_name,
            method_name: method_name.to_string(),
            impl_def: None,
            impl_name: param.to_string(),
            impl_type_id: None,
            self_kind: info.self_kind,
            return_type: info.return_type,
            param_types: info.param_types,
            is_type_param_receiver: true,
        }
    }

    /// `method_name` of the operator impl `found`.
    pub(super) fn of_operator_impl(
        tysys: &TypeSystem,
        found: OperatorImpl,
        method_name: &str,
    ) -> Self {
        let OperatorImpl {
            info,
            impl_name,
            impl_type_id,
        } = found;
        Self {
            method_def: tysys.declared_method(info.impl_def, method_name),
            trait_name: info.trait_name,
            method_name: method_name.to_string(),
            impl_def: Some(info.impl_def),
            impl_name,
            impl_type_id: Some(impl_type_id),
            self_kind: info.self_kind,
            return_type: info.output_type,
            param_types: info.rhs_type.into_iter().collect(),
            is_type_param_receiver: false,
        }
    }
}

/// An operator trait impl, with the type it was found on: the receiver's own,
/// or a newtype's base where dispatch fell back to it.
pub(super) struct OperatorImpl {
    pub(super) info: ArithmeticTraitInfo,
    pub(super) impl_name: String,
    pub(super) impl_type_id: TypeId,
}

/// A `From<Array<E>>` impl a literal can coerce through.
#[derive(Clone)]
pub(super) struct FromArrayInfo {
    /// The `impl From<Array<…>>` block that matched. `None` for an `Array<E>`
    /// target, which runs no conversion.
    pub(super) impl_def: Option<DefId>,
    /// `E` — the type every element (or key-value pair) of the literal takes.
    pub(super) element_type: TypeId,
    /// `Array<E>` — the value the literal materializes and `from` receives.
    pub(super) array_type: TypeId,
    /// Module that wrote the impl block.
    pub(super) impl_module_source: ModuleSource,
    /// `From<Array<…>>` as the impl block declares it — the spelling the
    /// method template is registered under, so a generic impl's argument is
    /// still written in its own parameters (`From<Array<T>>`).
    pub(super) trait_name: FqTraitName,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compiler_host::{Code, Diagnostic};
    use crate::token::Span;
    use std::assert_matches;

    fn span() -> Span {
        Span::new(0, 1, 7, 3)
    }

    #[test]
    fn diagnostic_carries_message_and_span() {
        let err = TypeError::UnknownType {
            name: "Frobnicate".to_string(),
            span: span(),
        };

        let diag: Diagnostic = err.into();
        assert_eq!(diag.message, "unknown type 'Frobnicate'");
        let diag_span = diag.span.expect("span");
        assert_eq!((diag_span.line, diag_span.column), (7, 3));
    }

    #[test]
    fn render_carries_diagnostic_code_and_span() {
        let (code, message, sp) = TypeError::ResumeOutsideHandler { span: span() }.render();
        assert_matches!(code, Code::UnsupportedFeature);
        assert_eq!(
            message,
            "`resume` is only valid inside an effect handler method body"
        );
        assert_eq!((sp.line, sp.column), (7, 3));
    }
}
