//! Function and struct names: a free function, a method, the receiver it
//! mangles, and the identity a call graph keys on.

use super::type_name::{DeclName, DeclaredHead, FqTraitName, FqTypeName, MangledName, TypeHead};
use super::{
    CLOSURE_CALL_METHOD, CLOSURE_STRUCT_PREFIX, UNCHECKED_TWIN_SUFFIX, before_dae, checked_name,
    mangle_generic_name, unchecked_twin_name,
};
use crate::defs::DefId;
use crate::module_source::ModuleSource;
use crate::tir::ResolvedType;
use crate::{ast, tir};
use std::fmt;

/// A free function name (not a method on a struct).
///
/// Format: `{module_source}/{name}`
///
/// Examples:
/// - `geometry.wado/helper`
/// - `core:rt/log_stdout`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FreeFunctionName {
    /// The module where the function is defined
    pub module_source: ModuleSource,
    /// The function name (e.g., `helper`)
    pub name: String,
}

impl FreeFunctionName {
    pub fn new(module_source: ModuleSource, name: String) -> Self {
        Self {
            module_source,
            name,
        }
    }

    /// Create a `FreeFunctionName` from `ModuleSource` and name.
    pub fn from_module_source(module_source: &ModuleSource, name: &str) -> Self {
        Self {
            module_source: module_source.clone(),
            name: name.to_string(),
        }
    }
}

impl fmt::Display for FreeFunctionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.module_source, self.name)
    }
}

/// A method name on a struct.
///
/// Format:
/// - Without trait: `{struct_name}::{method_name}`
/// - With trait: `{struct_name}^{trait_name}::{method_name}`
///
/// Examples:
/// - `geometry.wado/Point::sum`
/// - `geometry.wado/Point^Display::fmt`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct MethodName {
    /// The module source where the method is defined
    pub module_source: ModuleSource,
    /// The receiver this method hangs off (e.g., `./geom.wado/Point`).
    pub struct_name: FqTypeName,
    /// The trait this is an implementation of, named by the module that
    /// declares it (e.g. `core:prelude/fmt.wado/Display`).
    pub trait_name: Option<FqTraitName>,
    /// The method name (e.g., `sum`)
    pub method_name: String,
}

impl MethodName {
    pub fn new(
        module_source: ModuleSource,
        struct_name: FqTypeName,
        trait_name: Option<FqTraitName>,
        method_name: String,
    ) -> Self {
        Self {
            module_source,
            struct_name,
            trait_name,
            method_name,
        }
    }

    /// Returns the local part of the method name without the module path.
    /// Format: `Struct^Trait::method` or `Struct::method`
    pub fn local_name(&self) -> String {
        Self::format_local(
            &self.struct_name,
            self.trait_name.as_ref(),
            &self.method_name,
        )
    }

    /// Format a local method name (without module path).
    /// This is the canonical way to build method names like `Struct^Trait::method`.
    pub fn format_local(
        struct_name: &FqTypeName,
        trait_name: Option<&FqTraitName>,
        method_name: &str,
    ) -> String {
        match trait_name {
            Some(trait_n) => format!("{struct_name}^{trait_n}::{method_name}"),
            None => format!("{struct_name}::{method_name}"),
        }
    }

    /// A mangled receiver with its optional trait: `Struct<TypeArgs>^Trait`.
    pub fn format_struct_with_trait(struct_part: &str, trait_name: Option<&FqTraitName>) -> String {
        match trait_name {
            Some(trait_n) => format!("{struct_part}^{trait_n}"),
            None => struct_part.to_string(),
        }
    }

    /// Join a struct part (which may include ^Trait) with a method part.
    /// This is the final step of method name construction.
    pub fn join_struct_method(struct_part: &str, method_part: &str) -> String {
        format!("{struct_part}::{method_part}")
    }

    /// Format a method name with type arguments.
    /// Format: `method<TypeArgs>` or `method`
    pub fn format_method_with_args(method_name: &str, type_args: &[String]) -> String {
        if type_args.is_empty() {
            method_name.to_string()
        } else {
            format!("{}<{}>", method_name, type_args.join(","))
        }
    }
}

impl fmt::Display for MethodName {
    /// The receiver already names the module that declares it, so nothing is
    /// prefixed here. [`Self::module_source`] — where the *impl* lives, which
    /// need not be the receiver's module — is printed by whoever needs it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.local_name())
    }
}

/// Parsed components of a local method name (without module path).
///
/// This is used to extract struct/trait/method info from names like:
/// - `Point::sum` → `struct_name="Point"`, `trait_name=None`, `method_name="sum"`
/// - `Point^Display::fmt` → `struct_name="Point"`, `trait_name=Some("Display")`, `method_name="fmt"`
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LocalMethodName {
    /// The typed receiver shape (`Point`, `&T`, `S::SeqSerializer`). Its
    /// `head_key()` is the base identity string preserved across monomorphization.
    ///
    /// With [`Self::struct_type_args`] this is the whole of the receiver's
    /// identity — the mangled `Point<i32>` spelling is [`Self::struct_name`],
    /// derived rather than stored, so the two cannot disagree.
    pub receiver: Receiver,
    /// The trait / effect / resource this method implements, named by the
    /// module that declares it — `None` for an inherent method.
    ///
    /// One field, not three. The declaring module, the declaration name and
    /// the instantiated spelling are all rendered from it, so no two of them
    /// can disagree about which trait this is.
    pub trait_name: Option<FqTraitName>,
    /// Concrete `TypeId`s of the trait / resource type arguments at this
    /// impl site (e.g. `[u8]` for `impl Stream<u8> for MockCM`). Empty
    /// for non-generic traits / effects, and for the bare base form
    /// recorded outside of impl-block method context. The dispatch
    /// synthesis consumes this to produce **per-monomorphisation**
    /// dispatch infrastructure: each unique `(base_trait, trait_type_args)`
    /// pair gets its own `$Dispatch_<R>__<args>` struct + global +
    /// per-op wrappers, with the resource's operation types substituted
    /// for that combination.
    pub trait_type_args: Vec<tir::TypeId>,
    /// The receiver's type arguments, structured. Together with `receiver`
    /// they *are* `struct_name`: [`Self::fq_struct_name`] rebuilds the
    /// instantiated receiver from them rather than reading the mangled
    /// `struct_name` back apart.
    pub struct_type_args: Vec<FqTypeName>,
    /// The method name (e.g., "sum" or "fmt")
    pub method_name: String,
    /// Method-level type args (e.g., `[i64]` for `transform<i64>`), structured.
    /// [`Self::full_method_name`] renders them; nothing stores the rendering,
    /// so a call site's spelling and a definition's cannot drift.
    pub method_type_args: Vec<FqTypeName>,
    /// Whether the struct name is a type parameter that should be substituted directly
    /// during monomorphization (e.g., `T^Ord::cmp` where T should become i32).
    pub is_type_param_receiver: bool,
    /// The parameter a type-parameter receiver is, where it is one rather than
    /// a projection. Its spelling cannot say which: a method's own `T` may
    /// shadow its impl's.
    pub receiver_param: Option<tir::TypeParamId>,
    /// Whether this method came from an `impl Trait for &T` / `&mut T` block —
    /// a semantic flag (self-adjustment, codegen), distinct from the receiver's
    /// head being `&` (which `receiver` records).
    pub is_ref_impl: bool,
    /// CM canonical name from `#[cm("...")]` attribute on resource methods.
    /// When set, synthesis generates a CM binding function and rewrites
    /// the call site to use it instead of the original resource method.
    pub cm_name: Option<String>,
}

/// The reference kind of a `&` / `&mut` method receiver.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RefKind {
    Shared,
    Mut,
}

impl RefKind {
    /// The mangle prefix — the sole spelling of `&` / `&mut` from a `RefKind`.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            RefKind::Shared => "&",
            RefKind::Mut => "&mut",
        }
    }

    /// The ref kind of an AST type, or `None` for a non-reference.
    #[must_use]
    pub fn from_ast(ty: &ast::Type) -> Option<Self> {
        match ty {
            ast::Type::Reference(_) => Some(RefKind::Shared),
            ast::Type::MutReference(_) => Some(RefKind::Mut),
            _ => None,
        }
    }

    /// The ref kind of a resolved type, or `None` for a non-reference.
    #[must_use]
    pub fn from_resolved(ty: &ResolvedType) -> Option<Self> {
        match ty {
            ResolvedType::Ref(_) => Some(RefKind::Shared),
            ResolvedType::MutRef(_) => Some(RefKind::Mut),
            _ => None,
        }
    }
}

/// The shape of a method receiver, typed so nothing inspects a mangled name to
/// recover it. The `&` / `&mut` / `::` spellings appear only in this type's
/// `mangle` / `head_key`; producers build it from typed AST / resolved types.
///
/// Whether the receiver is a generic type parameter to substitute is an
/// orthogonal concern (a `Projection` head can be a param too), kept as
/// `LocalMethodName::is_type_param_receiver` rather than a variant here.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Receiver {
    /// A named type or type parameter: `Point`, `List`, `T`.
    Type(FqTypeName),
    /// A universal reference receiver `&T` / `&mut T`; the pointee rides in the
    /// receiver's type-arg list, not here.
    Ref(RefKind),
    /// A reference to a named head (`&Item`, `&List`), which an `impl` for
    /// `&X` registers under; its arguments ride in the type-arg list.
    RefTo(RefKind, FqTypeName),
}

impl Receiver {
    /// The receiver an `impl` written for the reference `target` registers
    /// under: the referent's head, or the bare kind where a parameter stands.
    #[must_use]
    pub fn of_ref_impl(kind: RefKind, target: &FqTypeName) -> Self {
        Self::ref_to(target).unwrap_or(Receiver::Ref(kind))
    }

    /// [`Receiver::RefTo`] the head `target` refers to; `None` unless `target`
    /// is a reference to a named type.
    #[must_use]
    pub fn ref_to(target: &FqTypeName) -> Option<Self> {
        let (kind, referent) = target.split_reference()?;
        referent
            .binder_name()
            .is_none()
            .then(|| Receiver::RefTo(kind, referent.head_only()))
    }

    /// The canonical head string — identity key and mangle base. Module
    /// qualified, because that is what a mangled name embeds.
    #[must_use]
    pub fn head_key(&self) -> MangledName {
        match self {
            Receiver::Type(n) => MangledName::new(n.to_mangled()),
            Receiver::Ref(k) => MangledName::new(k.prefix()),
            Receiver::RefTo(..) => MangledName::new(self.mangle(&[])),
        }
    }

    /// The declaration this receiver names, or `None` for a reference or a head
    /// no module declares.
    #[must_use]
    pub fn def(&self) -> Option<DefId> {
        match self {
            Receiver::Type(fq) => fq.head().def(),
            Receiver::Ref(_) | Receiver::RefTo(..) => None,
        }
    }

    /// Whether this receiver names a declaration, whose type arguments stand
    /// one per declared parameter. A tuple, a reference or a projection spreads
    /// its parts over that list instead.
    #[must_use]
    pub fn is_declared_type(&self) -> bool {
        matches!(
            self,
            Receiver::Type(fq) | Receiver::RefTo(_, fq) if matches!(fq.head(), TypeHead::Declared(_))
        )
    }

    /// Whether this receiver is a function type, whose one position is itself.
    #[must_use]
    pub fn is_function(&self) -> bool {
        matches!(self, Receiver::Type(fq) if matches!(fq.head(), TypeHead::Function { .. }))
    }

    /// Whether this receiver is one of the binders `names` declares — how a
    /// blanket's receiver is recognized. By spelling, since a declaration list
    /// has no owning block to compare.
    #[must_use]
    pub fn is_declared_binder_of<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> bool {
        self.binder_spelling()
            .is_some_and(|spelling| names.into_iter().any(|n| n == spelling))
    }

    /// The binder's written spelling, `None` for every other receiver.
    fn binder_spelling(&self) -> Option<&str> {
        match self {
            Receiver::Type(fq) => match fq.head() {
                TypeHead::Binder { name, .. } => Some(name),
                TypeHead::Declared(_)
                | TypeHead::Shape { .. }
                | TypeHead::Builtin(_)
                | TypeHead::Unresolved(_)
                | TypeHead::Infer { .. }
                | TypeHead::Projection { .. }
                | TypeHead::Tuple
                | TypeHead::Function { .. } => None,
            },
            Receiver::Ref(_) | Receiver::RefTo(..) => None,
        }
    }

    /// Whether this receiver is a template's own type-parameter binder.
    ///
    /// A binder names no declaration, so it has no spelling in the declaration
    /// namespace — only the mangled one, where it is scoped to its template.
    #[must_use]
    pub fn is_binder(&self) -> bool {
        matches!(self, Receiver::Type(fq) if matches!(fq.head(), TypeHead::Binder { .. }))
    }

    /// Whether this receiver is the built-in tuple.
    #[must_use]
    pub fn is_tuple(&self) -> bool {
        matches!(self, Receiver::Type(fq) if matches!(fq.head(), TypeHead::Tuple))
    }

    /// The name an `impl` header writes its target as — no module, no type
    /// arguments.
    ///
    /// A different namespace from [`Self::head_key`], and keeping the two
    /// apart is the point: an impl-header scan compares against this one, and
    /// feeding it a mangled head matched nothing (silently losing a trait
    /// segment, a resource requirement, or a go-to-definition edge).
    #[must_use]
    pub fn decl_key(&self) -> DeclName {
        match self {
            Receiver::Type(n) => n.decl_name(),
            // Header scans bucket every reference target by its kind.
            Receiver::Ref(k) | Receiver::RefTo(k, _) => DeclName::new(k.prefix()),
        }
    }

    /// Mangle the receiver with its type args (`Point<i32>`, `&List<i32>`,
    /// `S::SeqSerializer`).
    #[must_use]
    pub fn mangle(&self, type_args: &[String]) -> String {
        let mut out = String::new();
        match self {
            Receiver::Type(_) => {
                out.push_str(&mangle_generic_name(
                    self.head_key().as_mangled_str(),
                    type_args,
                ));
            }
            // The single pointee is the argument a universal `&T` carries.
            Receiver::Ref(k) => match type_args {
                [pointee] => {
                    push_ref_prefix(&mut out, *k);
                    out.push_str(pointee);
                }
                _ => out.push_str(&mangle_generic_name(k.prefix(), type_args)),
            },
            Receiver::RefTo(k, referent) => {
                push_ref_prefix(&mut out, *k);
                out.push_str(&mangle_generic_name(&referent.to_mangled(), type_args));
            }
        }
        out
    }

    /// The receiver's reference kind, or `None` for a value receiver.
    #[must_use]
    pub fn ref_kind(&self) -> Option<RefKind> {
        match self {
            Receiver::Ref(k) | Receiver::RefTo(k, _) => Some(*k),
            Receiver::Type(_) => None,
        }
    }
}

/// Write a receiver's reference prefix. `&` binds directly to the pointee;
/// `&mut` is a word and needs the separator. [`Receiver::mangle`] and
/// `TypeTable::mangle_type_arg_for_generic` spell it the same way, and a
/// definition's name is built by one while its call sites go through the other.
pub(super) fn push_ref_prefix(out: &mut String, kind: RefKind) {
    out.push_str(kind.prefix());
    if kind == RefKind::Mut {
        out.push(' ');
    }
}

impl LocalMethodName {
    /// The declaration of the trait this method implements; `None` for an
    /// inherent method.
    #[must_use]
    pub fn trait_decl(&self) -> Option<DefId> {
        self.trait_name.as_ref().map(FqTraitName::called_decl)
    }

    /// The typed receiver shape — the query consumers use to reason about the
    /// receiver instead of parsing its mangled identity.
    #[must_use]
    pub fn receiver(&self) -> &Receiver {
        &self.receiver
    }

    /// The receiver as a mangled name embeds it: the head with its type
    /// arguments applied (`Point<i32>`, `&List<i32>`).
    ///
    /// Derived from [`Self::receiver`] and [`Self::struct_type_args`] rather
    /// than stored beside them, so it cannot drift from the structure it is a
    /// rendering of.
    #[must_use]
    pub fn struct_name(&self) -> String {
        if self.struct_type_args.is_empty() {
            self.receiver.head_key().into_string()
        } else {
            let rendered: Vec<String> = self
                .struct_type_args
                .iter()
                .map(FqTypeName::to_mangled)
                .collect();
            self.receiver.mangle(&rendered)
        }
    }

    /// [`Self::struct_name`] in the mangled namespace, so a consumer that keys
    /// on it cannot be handed a declaration name instead.
    #[must_use]
    pub fn mangled_struct_name(&self) -> MangledName {
        MangledName::new(self.struct_name())
    }

    /// The base receiver identity string: `Point`, `&`, `&mut`, `S::SeqSerializer`.
    #[must_use]
    pub fn base_struct_name(&self) -> String {
        self.receiver.head_key().into_string()
    }

    /// [`Self::base_struct_name`] as the receiver form a mangled name embeds —
    /// read straight off the typed receiver, never re-derived from a string.
    #[must_use]
    pub fn fq_base_struct_name(&self) -> FqTypeName {
        match &self.receiver {
            Receiver::Type(fq) => fq.clone(),
            // A `&` / `&mut` head names no declaration, so no module qualifies it.
            Receiver::Ref(kind) => FqTypeName::builtin(kind.prefix()),
            Receiver::RefTo(kind, referent) => referent.clone().with_reference(*kind),
        }
    }

    /// The receiver's declaration name — what an `impl` header writes. See
    /// [`Receiver::decl_key`] for why this is a different namespace from
    /// [`Self::base_struct_name`].
    #[must_use]
    pub fn receiver_decl_key(&self) -> DeclName {
        self.receiver.decl_key()
    }

    /// [`Self::struct_name`] as the receiver form a mangled name embeds —
    /// rebuilt from the typed receiver and its structured arguments.
    #[must_use]
    pub fn fq_struct_name(&self) -> FqTypeName {
        // A single-pointee reference receiver spells as the pointee carrying a
        // `&` / `&mut`, not as a `&` head applied to arguments — the same rule
        // `Receiver::mangle` follows, so this and [`Self::struct_name`]
        // render one spelling.
        if let Receiver::Ref(kind) = self.receiver
            && let [pointee] = self.struct_type_args.as_slice()
        {
            return pointee.clone().with_reference(kind);
        }
        self.fq_base_struct_name()
            .with_args(self.struct_type_args.clone())
    }

    /// The receiver's declaration name, without its module — the key form the
    /// WIT-derived CM interface registry uses (`Fields::new`). That registry
    /// knows nothing of Wado modules, so a static call on a WASI resource has
    /// to reach it by the declared name alone.
    #[must_use]
    pub fn receiver_decl_name(&self) -> DeclName {
        self.fq_base_struct_name().decl_name()
    }

    /// The receiver's reference kind, or `None` for a value receiver.
    #[must_use]
    pub fn ref_receiver(&self) -> Option<RefKind> {
        self.receiver.ref_kind()
    }

    /// Create a new `LocalMethodName` directly from components.
    ///
    /// IMPORTANT: `struct_name` must be the base struct name WITHOUT type parameters.
    /// Use `with_type_args()` or `with_struct_type_args()` to add type parameters.
    ///
    /// `trait_name` names the trait by its declaration, so an alias or a
    /// second module's same-named trait cannot reach the mangle.
    #[must_use]
    pub fn new(
        struct_name: FqTypeName,
        trait_name: Option<FqTraitName>,
        method_name: String,
    ) -> Self {
        Self::of(Receiver::Type(struct_name), trait_name, method_name)
    }

    /// Construct a method name for a `&T` / `&mut T` ref-impl receiver.
    #[must_use]
    pub fn new_ref(kind: RefKind, trait_name: Option<FqTraitName>, method_name: String) -> Self {
        Self::of(Receiver::Ref(kind), trait_name, method_name)
    }

    /// Construct from an explicit typed receiver — the single construction path;
    /// `new` / `new_ref` are thin typed wrappers. No string is inspected.
    #[must_use]
    pub fn of(receiver: Receiver, trait_name: Option<FqTraitName>, method_name: String) -> Self {
        Self {
            receiver,
            struct_type_args: Vec::new(),
            trait_name,
            trait_type_args: Vec::new(),
            method_name,
            method_type_args: vec![],
            is_type_param_receiver: false,
            receiver_param: None,
            is_ref_impl: false,
            cm_name: None,
        }
    }

    /// Whether this names the method's `unchecked` twin, which a call marked
    /// `unchecked` reaches. The suffix is the only record: no Wado identifier
    /// can spell its `$`, so a copied [`Self::method_name`] carries it along.
    #[must_use]
    pub fn is_unchecked(&self) -> bool {
        self.method_name.ends_with(UNCHECKED_TWIN_SUFFIX)
    }

    /// Point this name at the method's `unchecked` twin.
    pub fn mark_unchecked(&mut self) {
        assert!(!self.is_unchecked(), "{self:?} already names a twin");
        self.method_name = unchecked_twin_name(&self.method_name);
    }

    /// Point this name at the method a twin is the twin of.
    pub fn mark_checked(&mut self) {
        assert!(self.is_unchecked(), "{self:?} names no twin");
        self.method_name = self.declared_method_name().to_string();
    }

    /// The method as its declaration names it, where this name may name its
    /// `unchecked` twin.
    #[must_use]
    pub fn declared_method_name(&self) -> &str {
        checked_name(&self.method_name)
    }

    /// The trait's declaration name without type args (`"Display"`,
    /// `"Stream"`), which is what the by-name decl indices key on.
    #[must_use]
    pub fn base_trait_name(&self) -> Option<&str> {
        self.trait_name.as_ref().map(FqTraitName::base_name)
    }

    /// The trait this method implements. Two modules can each declare
    /// `interface Logger`; this is what tells their handler impls apart.
    #[must_use]
    pub fn base_trait_head(&self) -> Option<&DeclaredHead> {
        self.trait_name
            .as_ref()
            .and_then(FqTraitName::declared_head)
    }

    /// Create a new `LocalMethodName` with all components including method type args.
    ///
    /// IMPORTANT: `struct_name` must be the base struct name WITHOUT type parameters.
    #[must_use]
    pub fn with_method_type_args(
        struct_name: FqTypeName,
        trait_name: Option<FqTraitName>,
        method_name: String,
        method_type_args: Vec<FqTypeName>,
    ) -> Self {
        debug_assert!(
            struct_name.args().is_empty(),
            "LocalMethodName::with_method_type_args() expects a base receiver without type args, got: {struct_name}"
        );
        Self {
            receiver: Receiver::Type(struct_name),
            struct_type_args: Vec::new(),
            trait_name,
            trait_type_args: Vec::new(),
            method_name,
            method_type_args,
            is_type_param_receiver: false,
            receiver_param: None,
            is_ref_impl: false,
            cm_name: None,
        }
    }

    /// Create a version of this `LocalMethodName` with type args applied.
    ///
    /// `impl_type_args` are applied to the struct name
    /// (e.g., `"List"` + `["i32"]` → `"List<i32>"`).
    /// `method_type_args` are stored separately (not embedded in `method_name`).
    /// The base struct name and the trait are preserved (not changed by type
    /// args).
    #[must_use]
    pub fn with_type_args(
        &self,
        impl_type_args: &[FqTypeName],
        method_type_args: &[FqTypeName],
    ) -> Self {
        Self {
            struct_type_args: impl_type_args.to_vec(),
            receiver: self.receiver.clone(),
            trait_name: self.trait_name.clone(),
            trait_type_args: self.trait_type_args.clone(),
            method_name: self.method_name.clone(),
            method_type_args: method_type_args.to_vec(),
            is_type_param_receiver: self.is_type_param_receiver,
            receiver_param: self.receiver_param,
            is_ref_impl: self.is_ref_impl,
            cm_name: self.cm_name.clone(),
        }
    }

    /// Create a version with only struct type args (no method type args).
    /// This is a convenience method for the common case.
    #[must_use]
    pub fn with_struct_type_args(&self, type_args: &[FqTypeName]) -> Self {
        self.with_type_args(type_args, &[])
    }

    /// Create a version with the trait name mangled with type args.
    ///
    /// `trait_type_args` are applied to the trait (e.g. `Stream` + `["u8"]` →
    /// `Stream<u8>`). The trait's declaration and declaring module are
    /// untouched, so decl-index lookups still resolve against the bare
    /// declaration.
    ///
    /// Panics if `self.trait_name` is `None` — type args on an inherent
    /// method don't have a trait to mangle.
    #[must_use]
    pub fn with_trait_type_args(&self, trait_type_args: &[FqTypeName]) -> Self {
        let trait_name = self
            .trait_name
            .as_ref()
            .expect("with_trait_type_args() requires a trait name")
            .head_only()
            .with_args(trait_type_args.to_vec());
        Self {
            trait_name: Some(trait_name),
            ..self.clone()
        }
    }

    /// This method named at the concrete `receiver`: a type parameter's
    /// answer (`T^Ord::cmp` → `i32^Ord::cmp`), or what an `impl` block writes.
    /// A `&X` receiver is the reference to `X`'s head, carrying `X`'s
    /// arguments, so the reference kind survives for dispatch to read.
    #[must_use]
    pub fn at_owner(&self, receiver: &FqTypeName) -> Self {
        let (head, args) = match receiver.split_reference() {
            Some((kind, pointee)) => (
                Receiver::RefTo(kind, pointee.head_only()),
                pointee.args().to_vec(),
            ),
            None => (
                Receiver::Type(receiver.head_only()),
                receiver.args().to_vec(),
            ),
        };
        Self {
            receiver: head,
            struct_type_args: args,
            is_type_param_receiver: false,
            receiver_param: None,
            ..self.clone()
        }
    }

    /// Get the full method name including type args (e.g., `"transform<i64>"`)
    #[must_use]
    pub fn full_method_name(&self) -> String {
        self.method_with_args(FqTypeName::to_mangled)
    }

    fn method_with_args(&self, spell: fn(&FqTypeName) -> String) -> String {
        let args: Vec<String> = self.method_type_args.iter().map(spell).collect();
        MethodName::format_method_with_args(&self.method_name, &args)
    }

    /// Generate the mangled name from the components.
    ///
    /// Produces:
    /// - `StructName::method` for inherent methods
    /// - `StructName^TraitName::method` for trait methods
    /// - `StructName<TypeArgs>::method` for monomorphized methods
    /// - `StructName<TypeArgs>^TraitName::method` for monomorphized trait methods
    #[must_use]
    pub fn to_mangled_name(&self) -> String {
        let method_part = self.full_method_name();
        if let Some(trait_name) = &self.trait_name {
            format!("{}^{}::{}", self.struct_name(), trait_name, method_part)
        } else {
            format!("{}::{}", self.struct_name(), method_part)
        }
    }

    /// [`Self::to_mangled_name`] as source spells it: the receiver, the trait
    /// and the method's type arguments lose their declaring modules.
    ///
    /// The display counterpart of the mangle, derived from the same fields —
    /// so a reader wanting `String::len` out of
    /// `core:prelude/string.wado/String::len` asks the structure rather than
    /// splitting the rendering back apart.
    #[must_use]
    pub fn to_display_name(&self) -> String {
        let receiver = self.fq_struct_name().to_display();
        let method = self.method_with_args(FqTypeName::to_display);
        match &self.trait_name {
            Some(trait_name) => format!("{receiver}^{}::{method}", trait_name.to_display()),
            None => format!("{receiver}::{method}"),
        }
    }

    /// A monomorphization-invariant identity: base struct / trait names and the
    /// bare method name, every type argument dropped, so a generic method and its
    /// instantiations share one key where [`Self::to_mangled_name`] would not.
    ///
    /// For keying a property of the method rather than of the instantiation —
    /// whether it takes `self` by value, say.
    pub fn base_dispatch_key(&self) -> String {
        match &self.trait_name {
            Some(trait_name) => {
                format!(
                    "{}^{}::{}",
                    self.base_struct_name(),
                    trait_name.head_only(),
                    self.method_name
                )
            }
            None => format!("{}::{}", self.base_struct_name(), self.method_name),
        }
    }

    /// True for the synthesized `$call` on a `$Closure_N` functor struct.
    /// Syntactically these are inherent methods, but they dispatch through the
    /// closure's canonical type, whose Wasm signature is fixed — so a caller
    /// that reshapes ABIs must filter them out or the signature will no longer
    /// match the vtable slot they are installed into.
    pub fn is_closure_call(&self) -> bool {
        before_dae(&self.method_name) == CLOSURE_CALL_METHOD
            && self
                .fq_struct_name()
                .decl_name()
                .as_decl_str()
                .starts_with(CLOSURE_STRUCT_PREFIX)
    }
}

/// A unified function identifier that can be either a free function or a method.
///
/// Not boxed despite the size gap: `optimize/dce.rs` builds these per call-graph
/// edge, where an allocation costs more than the stack bytes it saves.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FunctionId {
    Free(FreeFunctionName),
    Method(MethodName),
}

impl FunctionId {
    /// The identity a call to a free function resolves by.
    #[must_use]
    pub fn free(module_source: &ModuleSource, name: &str) -> Self {
        Self::Free(FreeFunctionName::from_module_source(module_source, name))
    }
}

impl fmt::Display for FunctionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FunctionId::Free(free) => write!(f, "{free}"),
            FunctionId::Method(method) => write!(f, "{method}"),
        }
    }
}

impl From<FreeFunctionName> for FunctionId {
    fn from(free: FreeFunctionName) -> Self {
        FunctionId::Free(free)
    }
}

impl From<MethodName> for FunctionId {
    fn from(method: MethodName) -> Self {
        FunctionId::Method(method)
    }
}

/// A qualified struct type name, `{module}/{name}` — e.g.
/// `geometry.wado/Point`, `core:rt/SomeType`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StructName {
    /// The module where the struct is defined
    pub module_source: ModuleSource,
    /// The struct name (e.g., `Point`)
    pub name: String,
}

impl StructName {
    #[must_use]
    pub fn new(module_source: ModuleSource, name: String) -> Self {
        Self {
            module_source,
            name,
        }
    }
}

impl fmt::Display for StructName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.module_source, self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hashmap::IndexSet;
    use crate::module_source::ModuleSourceInterner;

    #[test]
    fn a_function_is_qualified_by_its_module_name() {
        let mut interner = ModuleSourceInterner::new();
        let func = FreeFunctionName::new(interner.core("cli"), "println".to_string());
        assert_eq!(func.to_string(), "core:cli/println");

        let entry = FreeFunctionName::from_module_source(
            &interner.entry_point("/abs/pkg/src/main.wado"),
            "main",
        );
        assert_eq!(entry.to_string(), "main.wado/main");
    }

    #[test]
    fn test_struct_name_to_string() {
        let mut interner = ModuleSourceInterner::new();
        let struct_name = StructName::new(interner.local("./geometry.wado"), "Point".to_string());
        assert_eq!(struct_name.to_string(), "geometry.wado/Point");
    }

    #[test]
    fn test_struct_name_hash_eq() {
        let mut interner = ModuleSourceInterner::new();
        let mut point_in = |path: &str| StructName::new(interner.local(path), "Point".to_string());
        let s1 = point_in("./geometry.wado");
        let s2 = point_in("./geometry.wado");
        let s3 = point_in("./other.wado");

        let mut set = IndexSet::default();
        set.insert(s1);
        assert!(set.contains(&s2));
        assert!(!set.contains(&s3));
    }

    #[test]
    fn test_receiver_mangle() {
        let shared = Receiver::Ref(RefKind::Shared);
        assert_eq!(shared.mangle(&["i32".into()]), "&i32");
        assert_eq!(shared.mangle(&["List<i32>".into()]), "&List<i32>");
        let mutable = Receiver::Ref(RefKind::Mut);
        assert_eq!(mutable.mangle(&["String".into()]), "&mut String");
        assert_eq!(mutable.mangle(&["List<i32>".into()]), "&mut List<i32>");
        // Named receivers fall through to mangle_generic_name.
        assert_eq!(
            Receiver::Type(FqTypeName::builtin("List")).mangle(&["i32".into()]),
            "List<i32>"
        );
        // A reference to a named head keeps the head, its arguments applied.
        let list = FqTypeName::builtin("List");
        let shared_list = Receiver::RefTo(RefKind::Shared, list.clone());
        assert_eq!(shared_list.mangle(&["i32".into()]), "&List<i32>");
        assert_eq!(shared_list.head_key().as_mangled_str(), "&List");
        let mut_list = Receiver::RefTo(RefKind::Mut, list);
        assert_eq!(mut_list.mangle(&["i32".into()]), "&mut List<i32>");
        assert_eq!(mut_list.mangle(&[]), "&mut List");
    }

    #[test]
    fn test_ref_impl_receiver_names_its_referent() {
        let item = FqTypeName::builtin("Item");
        let template = LocalMethodName::of(
            Receiver::of_ref_impl(
                RefKind::Shared,
                &item.clone().with_reference(RefKind::Shared),
            ),
            None,
            "eq".into(),
        );
        assert_eq!(template.to_mangled_name(), "&Item::eq");
        let instance = template.with_struct_type_args(&[FqTypeName::builtin("i32")]);
        assert_eq!(instance.to_mangled_name(), "&Item<i32>::eq");
        assert_eq!(instance.fq_struct_name().to_mangled(), "&Item<i32>");
        // A type parameter behind the reference leaves the bare kind.
        let blanket = Receiver::of_ref_impl(
            RefKind::Mut,
            &FqTypeName::binder("T").with_reference(RefKind::Mut),
        );
        assert_eq!(blanket, Receiver::Ref(RefKind::Mut));
        let at_owner = template.at_owner(
            &item
                .with_args(vec![FqTypeName::builtin("bool")])
                .with_reference(RefKind::Shared),
        );
        assert_eq!(
            at_owner,
            instance.with_struct_type_args(&[FqTypeName::builtin("bool")])
        );
    }
}
