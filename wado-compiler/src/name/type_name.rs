//! Type names: how a type mangles and displays, and the fully qualified names
//! of types and traits.

use super::function::{RefKind, push_ref_prefix};
use super::{INTERNAL_PREFIX, effect_namespace, mangle_local_item_name};
use crate::ast::AstId;
use crate::defs::{DefId, DefKind, DefTable};
use crate::module_source::ModuleSource;
use crate::primitive::PrimitiveType;
use crate::tir;
use std::fmt;

/// Information about a type for name formatting.
///
/// This enum represents the structure of a type without requiring
/// knowledge of `TypeId` or `ResolvedType`. It serves as the interface
/// between type resolution (in tir.rs) and name formatting (here).
#[derive(Debug, Clone)]
pub enum TypeNameInfo {
    /// A primitive type (i32, f64, bool, etc.)
    Primitive(String),
    /// The unit type ()
    Unit,
    /// A named type (struct, enum, variant, resource, newtype, type param)
    Named(String),
    /// A generic instance with type argument names already resolved
    Generic { name: String, args: Vec<String> },
    /// A built-in tuple `[T1, T2, …]` with element names already resolved
    Tuple(Vec<String>),
    /// `Option<T>` with inner type name
    Option(String),
    /// `Array<T>` (raw Wasm GC array, NOT the user-facing `List<T>` struct)
    BuiltinArray(String),
    /// A reference type - formats as inner type (references stripped)
    Ref(String),
    /// Never, Unknown, or Error types
    Unknown,
}

/// Format a type name from its structural info.
///
/// This function centralizes all type name formatting logic.
/// Other modules should use this instead of formatting type names directly.
#[must_use]
pub fn format_type_name(info: TypeNameInfo) -> String {
    match info {
        TypeNameInfo::Primitive(name) => name,
        TypeNameInfo::Unit => UNIT_TYPE_NAME.to_string(),
        TypeNameInfo::Named(name) => name,
        TypeNameInfo::Generic { name, args } => mangle_generic_name(&name, &args),
        TypeNameInfo::Tuple(elems) => mangle_tuple_type(&elems),
        TypeNameInfo::Option(inner) => mangle_option_type(&inner),
        TypeNameInfo::BuiltinArray(elem) => mangle_builtin_array_type(&elem),
        TypeNameInfo::Ref(inner) => inner,
        TypeNameInfo::Unknown => "unknown".to_string(),
    }
}

/// Build a monomorphized type name: `("Box", ["i32"])` → `"Box<i32>"`. The tuple
/// head is spelled `[i32,i32]`, not `[]<i32,i32>` — the one spelling
/// [`FqTypeName::to_mangled`] gives it, so an instantiated tuple receiver and a
/// concrete `impl Trait for [i32, i32]` name the same function.
pub fn mangle_generic_name(base_name: &str, type_args: &[String]) -> String {
    if type_args.is_empty() {
        base_name.to_string()
    } else if base_name == TUPLE_TYPE_NAME {
        mangle_tuple_type(type_args)
    } else {
        format!("{}<{}>", base_name, type_args.join(","))
    }
}

/// Mangle a tuple as `[T1,T2,…]`, e.g. `["i32","String"]` → `"[i32,String]"`.
pub fn mangle_tuple_type(elems: &[String]) -> String {
    format!("[{}]", elems.join(","))
}

/// The head name of a tuple type, which no Wado identifier can spell.
pub const TUPLE_TYPE_NAME: &str = "[]";

/// The name of the unit type: its source spelling.
pub const UNIT_TYPE_NAME: &str = "()";

/// The name of the never type: its source spelling.
pub const NEVER_TYPE_NAME: &str = "!";

/// A name as source and an `impl` header spell it, which by-name declaration
/// lookups key on; unlike a mangled name, it carries no module.
// No `Deref`, `AsRef<str>` or `From<String>`: mixing the two namespaces must not compile.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeclName(String);

impl DeclName {
    /// Mint from a name already known to be in the declaration namespace —
    /// read off a declaration, an `impl` header, or an import scope. Restricted
    /// to this crate so the namespace has a bounded set of entry points.
    #[must_use]
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The underlying spelling. Named rather than a `Deref` so a consumer that
    /// wants a mangled name cannot reach one by accident.
    #[must_use]
    pub fn as_decl_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for DeclName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A name in the *mangled* namespace: module-qualified, type arguments
/// rendered — what an emitted function name embeds and what `func_map` keys on.
///
/// The counterpart to [`DeclName`], and deliberately not convertible to it. A
/// mangled head fed to a declaration lookup matched nothing and silently lost a
/// trait segment, a resource requirement, or a go-to-definition edge; the two
/// being interchangeable `String`s is what let that compile.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MangledName(String);

impl MangledName {
    /// Mint from a spelling already in the mangled namespace — produced by
    /// [`FqTypeName::to_mangled`] or by a mangler that qualifies the same way.
    #[must_use]
    pub(crate) fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// A function's project-wide key: the module it is emitted in and the name
    /// it is emitted under. The one way `func_map` keys are built, so a caller
    /// cannot assemble the pair in a namespace the map does not store.
    #[must_use]
    pub fn in_module(module: &ModuleSource, local_name: &str) -> Self {
        Self(format!("{module}/{local_name}"))
    }

    /// The `wasi/<name>` key a WASI canonical import is registered under.
    #[must_use]
    pub fn wasi_import(name: &str) -> Self {
        Self(format!("wasi/{name}"))
    }

    #[must_use]
    pub fn as_mangled_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for MangledName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A `Type::method` path in the declaration namespace — the key the CM
/// interface registry and the import-adapter map store.
///
/// Built from a [`DeclName`] receiver rather than assembled with `format!` at
/// each call site, so the half that decides the namespace cannot be a mangled
/// name. Two of the three sites that assembled this by hand were passing
/// `head_key()`, which the registry never stores.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeclPath(String);

impl DeclPath {
    /// `<receiver>::<method>` — a method reached through its declaring type.
    #[must_use]
    pub fn method_of(receiver: &DeclName, method: &str) -> Self {
        Self(format!("{receiver}::{method}"))
    }

    #[must_use]
    pub fn as_decl_str(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for DeclPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Comparing against a literal is sound in either direction: a literal spells
/// one name, and for the builtins these comparisons ask about (`u128`, `i128`)
/// the declaration and mangled namespaces coincide. What the newtype prevents
/// is a `DeclName` being *passed* where a mangled name is expected.
impl PartialEq<str> for DeclName {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for DeclName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl PartialEq<String> for DeclName {
    fn eq(&self, other: &String) -> bool {
        self.0 == *other
    }
}

/// Build a monomorphized method name from struct name, type args, and method name.
///
/// Examples:
/// - `mangle_method_generic("Box", &["i32"], "get")` → `"Box<i32>::get"`
/// - `mangle_method_generic("List", &["String"], "len")` → `"List<String>::len"`
pub fn mangle_method_generic(struct_name: &str, type_args: &[String], method_name: &str) -> String {
    let mangled_struct = mangle_generic_name(struct_name, type_args);
    format!("{mangled_struct}::{method_name}")
}

/// Spell a function type the way source does, minus the optional whitespace.
///
/// A function type names no declaration, so the rendering *is* the identity
/// and owes injectivity over everything `ResolvedType::Function` interns on.
/// `fn()->fn()with(E)` reads two ways — the effects on the inner type or on the
/// outer — so a function-typed return is parenthesised and the two spellings
/// part. The `with` members are always parenthesised, so a single member cannot
/// run into what follows it.
///
/// Examples:
/// - `mangle_fn_type(false, &["i32"], "i32", false, &[])` → `"fn(i32)->i32"`
/// - `mangle_fn_type(true, &[], "()", false, &["wasi:cli/Stdout".into()])`
///   → `"fn mut()->()with(wasi:cli/Stdout)"`
pub fn mangle_fn_type(
    is_mut: bool,
    params: &[String],
    ret_type: &str,
    return_is_function: bool,
    with_clause: &[String],
) -> String {
    let mut out = String::from(fn_keyword(is_mut));
    out.push('(');
    out.push_str(&params.join(","));
    out.push_str(")->");
    if return_is_function {
        out.push('(');
        out.push_str(ret_type);
        out.push(')');
    } else {
        out.push_str(ret_type);
    }
    if !with_clause.is_empty() {
        out.push_str("with(");
        out.push_str(&with_clause.join(","));
        out.push(')');
    }
    out
}

/// How a function type is spelled from its parts, as [`mangle_fn_type`] and
/// [`display_fn_type`] take them.
type FnTypeSpeller = fn(bool, &[String], &str, bool, &[String]) -> String;

/// Spell a function type for a diagnostic, as source writes it:
/// `fn(i32, String) -> bool with (Stdout, Stderr)`. A function-typed return is
/// parenthesised for the reason [`mangle_fn_type`] gives.
#[must_use]
pub fn display_fn_type(
    is_mut: bool,
    params: &[String],
    ret_type: &str,
    return_is_function: bool,
    with_clause: &[String],
) -> String {
    let ret_type = if return_is_function {
        format!("({ret_type})")
    } else {
        ret_type.to_string()
    };
    let clause = match with_clause {
        [] => String::new(),
        [one] => format!(" with {one}"),
        many => format!(" with ({})", many.join(", ")),
    };
    format!(
        "{}({}) -> {ret_type}{clause}",
        fn_keyword(is_mut),
        params.join(", ")
    )
}

fn fn_keyword(is_mut: bool) -> &'static str {
    if is_mut { "fn mut" } else { "fn" }
}

/// An effect as a [`mangle_fn_type`] `with` member. A concrete one carries its
/// declaring module, since two modules may declare one name.
fn mangle_effect_ref(effect: &tir::EffectRef) -> String {
    match effect {
        tir::EffectRef::Concrete(_) | tir::EffectRef::Undeclared { .. } => {
            let module = effect
                .module()
                .expect("a ground effect is named in a module");
            effect_namespace(module, effect.name())
        }
        tir::EffectRef::Param(_) | tir::EffectRef::Var { .. } => effect.name().to_string(),
    }
}

/// Whether `name` is a `fn(..)` type's spelling, as [`mangle_fn_type`] writes
/// it — the receiver a closure value dispatches through.
#[must_use]
pub fn is_fn_type_name(name: &str) -> bool {
    name.starts_with("fn(") || name.starts_with("fn mut(")
}

/// Build an Option type name from inner type name.
///
/// Examples:
/// - `mangle_option_type("i32")` → `"Option<i32>"`
fn mangle_option_type(inner_type: &str) -> String {
    format!("Option<{inner_type}>")
}

/// Build the raw GC array type name from element type name.
///
/// Examples:
/// - `mangle_builtin_array_type("i32")` → `"Array<i32>"`
pub fn mangle_builtin_array_type(elem_type: &str) -> String {
    format!("{ARRAY_TYPE_NAME}<{elem_type}>")
}

/// The head name of the raw GC array (`ResolvedType::BuiltinArray`): both the
/// `Array<T>` the resolver recognises and the method-owner base name
/// (`impl Array<T>` in `core:prelude/array.wado`) the dispatch arms report.
pub const ARRAY_TYPE_NAME: &str = "Array";

/// A declaration's mangled name, qualified by the module declaring it:
/// `{module}/{name}`.
pub fn mangle_declared(module: impl fmt::Display, name: &str) -> String {
    format!("{module}/{name}")
}

/// An associated-type projection's head, `T::Item`: one spelling for a
/// mangled name and a diagnostic alike.
pub fn assoc_type_projection_name(base: &str, assoc: &str) -> String {
    format!("{base}::{assoc}")
}

/// A reference type as source writes it, `&T` / `&mut T`: one spelling for a
/// mangled name and a diagnostic alike.
pub fn ref_type_name(kind: RefKind, inner: &str) -> String {
    let mut out = String::new();
    push_ref_prefix(&mut out, kind);
    out.push_str(inner);
    out
}

/// A type pack as source writes it: `..T`.
pub fn type_pack_name(name: &str) -> String {
    format!("..{name}")
}

/// What an element of pack `pack` names itself, bound by a walk inside `walk`
/// others: the pack's name, marked past the outermost walk so a nested walk's
/// element reads apart from the one enclosing it.
pub fn pack_element_name(pack: &str, walk: u32) -> String {
    if walk == 0 {
        pack.to_string()
    } else {
        format!("{pack}@{walk}")
    }
}

const REPEAT_PACK_MARKER: &str = "repeat";

/// The pack a signature mints for a `[..X]` repeating the non-pack `X`: its
/// length, which the argument settles.
pub fn repeat_pack_name(element: &str) -> String {
    format!("{INTERNAL_PREFIX}{REPEAT_PACK_MARKER}{INTERNAL_PREFIX}{element}")
}

/// The `X` a [`repeat_pack_name`] repeats, or `None` for any other name.
pub fn repeated_element(pack: &str) -> Option<&str> {
    pack.strip_prefix(INTERNAL_PREFIX)?
        .strip_prefix(REPEAT_PACK_MARKER)?
        .strip_prefix(INTERNAL_PREFIX)
}

/// A type parameter as a diagnostic names it: a minted repeat pack as the
/// `..X` that minted it.
pub fn written_param_name(param: &str) -> String {
    match repeated_element(param) {
        Some(element) => type_pack_name(element),
        None => param.to_string(),
    }
}

/// The `<A, B>` a diagnostic writes after a head; empty for no arguments.
fn display_type_args(args: &[String]) -> String {
    if args.is_empty() {
        String::new()
    } else {
        format!("<{}>", args.join(", "))
    }
}

/// A generic type as a diagnostic writes it: `Name<A, B>`.
pub fn display_generic_name(head: &str, args: &[String]) -> String {
    format!("{head}{}", display_type_args(args))
}

/// A tuple type as a diagnostic writes it: `[A, B]`.
pub fn display_tuple_type(elems: &[String]) -> String {
    format!("[{}]", elems.join(", "))
}

/// An associated-type projection as a diagnostic writes it: `T::Item<A>`, or
/// `<T as Trait<B>>::Item<A>` given its owning trait as written.
pub fn display_assoc_type_projection(
    base: &str,
    assoc: &str,
    args: &[String],
    owner: Option<&str>,
) -> String {
    let base = match owner {
        Some(owner) => format!("<{base} as {owner}>"),
        None => base.to_string(),
    };
    format!(
        "{}{}",
        assoc_type_projection_name(&base, assoc),
        display_type_args(args)
    )
}

/// Key of a declared type in the WIR type table.
///
/// Registration (`wir_build::types`) and lookup (`wir_build::context`) both
/// derive their key here so the two cannot drift apart.
///
/// Examples:
/// - `wir_type_key(core:prelude, "List<i32>")` → `"core:prelude//List<i32>"`
#[must_use]
pub fn wir_type_key(module_source: &ModuleSource, name: &str) -> String {
    format!("{module_source}//{name}")
}

/// Key of a plain `enum`, prefixed so a same-named variant keeps
/// [`wir_type_key`].
///
/// Examples:
/// - `wir_enum_type_key(app, "Color")` → `"app//enum:Color"`
#[must_use]
pub fn wir_enum_type_key(module_source: &ModuleSource, name: &str) -> String {
    format!("{module_source}//enum:{name}")
}

/// Key of a variant's per-case payload struct, derived from the variant's own
/// [`wir_type_key`].
///
/// Examples:
/// - `wir_variant_case_key("app//Shape", "Circle")` → `"app//Shape::Circle"`
#[must_use]
pub fn wir_variant_case_key(variant_key: &str, case_name: &str) -> String {
    format!("{variant_key}::{case_name}")
}

/// Key of a function type in the WIR type table.
///
/// Examples:
/// - `wir_func_type_key("app//run")` → `"functype//app//run"`
#[must_use]
pub fn wir_func_type_key(func_key: &str) -> String {
    format!("functype//{func_key}")
}

/// Key of an anonymous tuple struct in the WIR type table.
///
/// Examples:
/// - `wir_tuple_type_key(&["i32", "i32"])` → `"tuple//[i32, i32]"`
#[must_use]
pub fn wir_tuple_type_key(elems: &[String]) -> String {
    format!("{TUPLE_KEY_PREFIX}[{}]", elems.join(", "))
}

const TUPLE_KEY_PREFIX: &str = "tuple//";

/// Whether `name` spells a builtin shape, instantiated or not (`Array<u8>`).
/// See [`FqTypeName::builtin`].
#[must_use]
pub fn is_builtin_shape_name(name: &str) -> bool {
    fn head_of(name: &str) -> &str {
        match name.find('<') {
            Some(i) => &name[..i],
            None => name,
        }
    }
    let head = head_of(name);
    name.starts_with('&')
        || PrimitiveType::is_primitive_name(head)
        || matches!(
            head,
            UNIT_TYPE_NAME | NEVER_TYPE_NAME | ARRAY_TYPE_NAME | TUPLE_TYPE_NAME | "Fn"
        )
}

/// Whether `def` declares a builtin shape: a primitive, `Array` or the tuple
/// family, which every mangler spells bare.
#[must_use]
pub fn is_builtin_shape_decl(defs: &DefTable, def: DefId) -> bool {
    defs.kind(def) == DefKind::BuiltinType
}

/// A receiver name in the form a mangled name may embed, carrying the declaring
/// module — a bare `&str` cannot become one by accident. The mangled spelling is
/// a rendering ([`Self::to_mangled`]), never parsed back: a `ModuleSource` may
/// itself contain `/` and `<`, so no split is correct in general. Ask the fields.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FqTypeName {
    /// The `&` / `&mut` this name stands behind, outermost first. Keeping only
    /// the outer one spells `&&T` as `&T`, which is a different instantiation.
    reference: Vec<RefKind>,
    head: TypeHead,
    /// Type arguments, already fq themselves.
    args: Vec<FqTypeName>,
}

/// What an [`FqTypeName`]'s head names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TypeHead {
    /// A declaration. Equality reads the [`crate::defs::DefId`], so two
    /// same-named declarations are two heads however they are spelled.
    Declared(DeclaredHead),
    /// A shape no declaration names — an anonymous literal's, a closure
    /// environment's, a synthesised adapter's. Nothing declares it, so the
    /// rendering *is* the identity, scoped by the declaring module.
    Shape { module: ModuleSource, name: String },
    /// A shape no module declares — a primitive, `()`, `!`, the raw GC `Array`.
    /// Every mangler spells one the same way.
    Builtin(String),
    /// A written name that reaches no declaration, by its spelling. Resolving
    /// it reported the error, so nothing asks about it again.
    Unresolved(String),
    /// A type inference has yet to answer, with its spelling. No impl answers
    /// it until then.
    Infer { var: tir::InferVarId, name: String },
    /// A function type, its signature held as names so a binder inside one
    /// stays a binder and the solver can match it. The signature is the head:
    /// it is the shape itself, not an instantiation of one, so no name carries
    /// type arguments beside it.
    Function {
        is_mut: bool,
        /// The parameters, then the return.
        signature: Vec<FqTypeName>,
        effects: Vec<tir::EffectRef>,
    },
    /// A template's own type-parameter binder (`T`, a pack member `F`), with
    /// the `impl` block that binds it. Without `owner`, `impl<T: A> Tr for T`
    /// and `impl<T: B> Tr for T` share one template and one silently replaces
    /// the other (WEP 2026-08-12).
    Binder {
        name: String,
        owner: Option<BinderOwner>,
    },
    /// A tuple. Its own head, because a tuple is spelled `[a,b]` — not
    /// `Head<a,b>` like every other instantiated shape, which is what a
    /// `Builtin("[]")` carrying arguments would render as.
    Tuple,
    /// An associated type of another name (`T::Base`), answering a type once
    /// the base is concrete. Boxed, since every name carries a head.
    Projection(Box<ProjectionHead>),
}

/// What [`TypeHead::Projection`] projects off.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectionHead {
    base: FqTypeName,
    assoc: String,
    /// The trait declaring `assoc`, part of the identity: two traits declaring
    /// one name on a type bind it to different types (WEP-2026-08-12). `None`
    /// for a projection as written, before the bounds in force on its base
    /// say which trait declares the member: a bound's argument `Conv<T::Out>`.
    owning_trait: Option<DeclaredHead>,
    /// The arguments the trait is reached at, part of the identity too:
    /// `Holder<i32>`'s `Out` and `Holder<String>`'s are two types. `None`
    /// where a bare bound left one unwritten.
    trait_args: Option<Vec<FqTypeName>>,
}

impl TypeHead {
    /// A monomorphized instantiation, named by the fused spelling it mangles to
    /// (`List<…/Token>`). No declaration names one, so its rendering is its
    /// identity — the same rule [`Self::Shape`] carries.
    #[must_use]
    pub fn instance(module: &ModuleSource, mangled: &str) -> Self {
        Self::Shape {
            module: module.clone(),
            name: mangled.to_string(),
        }
    }

    /// The head's own name, as its declaration writes it. Diagnostics; a mangle
    /// takes [`Self::mangled`], which disambiguates a function-local
    /// declaration.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Declared(head) => head.name(),
            Self::Shape { name, .. }
            | Self::Builtin(name)
            | Self::Unresolved(name)
            | Self::Infer { name, .. }
            | Self::Binder { name, .. } => name,
            Self::Projection(head) => &head.assoc,
            Self::Tuple => TUPLE_TYPE_NAME,
            Self::Function { is_mut, .. } => fn_keyword(*is_mut),
        }
    }

    /// The spelling a mangle embeds — the head alone. What a binder's owner
    /// adds to it, [`FqTypeName::to_mangled`] adds, as it adds a projection's
    /// base.
    #[must_use]
    pub fn mangled(&self) -> &str {
        match self {
            Self::Declared(head) => head.mangled(),
            Self::Shape { name, .. }
            | Self::Builtin(name)
            | Self::Unresolved(name)
            | Self::Infer { name, .. }
            | Self::Binder { name, .. } => name,
            Self::Projection(head) => &head.assoc,
            Self::Tuple => TUPLE_TYPE_NAME,
            Self::Function { is_mut, .. } => fn_keyword(*is_mut),
        }
    }

    /// The declaration this head names, or `None` for a head that names none.
    /// This is identity: compare these, never [`Self::name`].
    #[must_use]
    pub fn def(&self) -> Option<DefId> {
        match self {
            Self::Declared(head) => Some(head.def()),
            Self::Shape { .. }
            | Self::Builtin(_)
            | Self::Unresolved(_)
            | Self::Infer { .. }
            | Self::Binder { .. }
            | Self::Projection { .. }
            | Self::Tuple
            | Self::Function { .. } => None,
        }
    }

    /// The declaring module, or `None` for a head no module declares.
    #[must_use]
    pub fn module(&self) -> Option<&ModuleSource> {
        match self {
            Self::Declared(head) => Some(head.module()),
            Self::Shape { module, .. } => Some(module),
            Self::Builtin(_)
            | Self::Unresolved(_)
            | Self::Infer { .. }
            | Self::Binder { .. }
            | Self::Projection { .. }
            | Self::Tuple
            | Self::Function { .. } => None,
        }
    }
}

impl FqTypeName {
    fn of_head_kind(head: TypeHead) -> Self {
        Self {
            reference: Vec::new(),
            head,
            args: Vec::new(),
        }
    }

    /// The tuple family is the one declaration whose spelling is not
    /// `Module/Head<args>` but bare `[a,b]`, and no impl is registered under
    /// the qualified form.
    #[must_use]
    pub fn declared(defs: &DefTable, def: DefId) -> Self {
        if defs.name(def) == TUPLE_TYPE_NAME {
            return Self::of_head_kind(TypeHead::Tuple);
        }
        Self::of_head_kind(TypeHead::Declared(DeclaredHead::new(defs, def)))
    }

    /// `module` and `name` are the pair the type table interns the shape under.
    #[must_use]
    pub fn shape(module: &ModuleSource, name: &str) -> Self {
        if name == TUPLE_TYPE_NAME {
            return Self::of_head_kind(TypeHead::Tuple);
        }
        if is_builtin_shape_name(name) {
            return Self::of_head_kind(TypeHead::Builtin(name.to_string()));
        }
        Self::of_head_kind(TypeHead::Shape {
            module: module.clone(),
            name: name.to_string(),
        })
    }

    /// A binder known only by its spelling (a name a synthesized template
    /// writes). Use [`Self::binder_of`] for a parameter an item declares — that
    /// one has an identity beyond its spelling.
    #[must_use]
    pub fn binder(name: &str) -> Self {
        Self::of_head_kind(TypeHead::Binder {
            name: name.to_string(),
            owner: None,
        })
    }

    /// The pack `name` the item `def` declares, spread in a tuple (`..T` in
    /// `[..T]`): any run of the tuple's elements, none included.
    #[must_use]
    pub fn pack_spread(defs: &DefTable, def: DefId, name: &str) -> Self {
        Self::binder_of(defs, def, &type_pack_name(name))
    }

    /// Whether this is a [`Self::pack_spread`].
    #[must_use]
    pub fn is_pack_spread(&self) -> bool {
        self.binder_name()
            .is_some_and(|name| name.starts_with(".."))
    }

    /// The type parameter `name` the item `def` declares (`T` in
    /// `impl<T: Bound> Trait for T`, `U` in `fn m<U>`). The one way to build
    /// this name, so no two callers can build two: a method's `U` shadowing its
    /// impl's is another binder however both are spelled.
    #[must_use]
    pub fn binder_of(defs: &DefTable, def: DefId, name: &str) -> Self {
        Self::of_head_kind(TypeHead::Binder {
            name: name.to_string(),
            owner: Some(BinderOwner::of_item(defs.module(def), defs.ast_id(def))),
        })
    }

    /// The parameter `name` of the generic associated type `assoc` that trait
    /// `def` declares (`E` in `type Buf<E>`). Owned by the family, so it names
    /// no other type, and an impl renaming it reaches this one.
    #[must_use]
    pub fn binder_of_family(defs: &DefTable, def: DefId, assoc: &str, name: &str) -> Self {
        Self::of_head_kind(TypeHead::Binder {
            name: name.to_string(),
            owner: Some(BinderOwner::of_family(
                defs.module(def),
                defs.ast_id(def),
                assoc,
            )),
        })
    }

    /// The associated type `assoc` of `base`, as `owning_trait` declares it
    /// (`T::Base`).
    #[must_use]
    pub fn projection(
        base: FqTypeName,
        assoc: &str,
        defs: &DefTable,
        owning_trait: DefId,
        trait_args: Option<Vec<FqTypeName>>,
    ) -> Self {
        Self::of_head_kind(TypeHead::Projection(Box::new(ProjectionHead {
            base,
            assoc: assoc.to_string(),
            owning_trait: Some(DeclaredHead::new(defs, owning_trait)),
            trait_args,
        })))
    }

    /// The associated type `assoc` of `base` as written, its declaring trait
    /// not yet settled (`T::Out` in a bound's arguments).
    #[must_use]
    pub fn written_projection(base: FqTypeName, assoc: &str) -> Self {
        Self::of_head_kind(TypeHead::Projection(Box::new(ProjectionHead {
            base,
            assoc: assoc.to_string(),
            owning_trait: None,
            trait_args: None,
        })))
    }

    /// A tuple of `elems`, spelled `[a,b]`.
    #[must_use]
    pub fn tuple(elems: Vec<FqTypeName>) -> Self {
        Self::of_head_kind(TypeHead::Tuple).with_args(elems)
    }

    /// A function type taking `params` and returning `ret`, its `with` clause
    /// `effects`.
    #[must_use]
    pub fn function(
        is_mut: bool,
        params: Vec<FqTypeName>,
        ret: FqTypeName,
        mut effects: Vec<tir::EffectRef>,
    ) -> Self {
        // An impl is found by its target's name, so a row's order must not
        // tell two names apart.
        tir::canonicalize_row(&mut effects);
        let mut signature = params;
        signature.push(ret);
        Self::of_head_kind(TypeHead::Function {
            is_mut,
            signature,
            effects,
        })
    }

    /// A builtin shape, spelled bare by every mangler: a primitive, `()`, `!`,
    /// `Array`, a reference. The tuple head becomes [`TypeHead::Tuple`].
    #[must_use]
    pub fn builtin(name: &str) -> Self {
        if name == TUPLE_TYPE_NAME {
            return Self::of_head_kind(TypeHead::Tuple);
        }
        Self::of_head_kind(TypeHead::Builtin(name.to_string()))
    }

    /// A written name that reaches no declaration.
    #[must_use]
    pub fn unresolved(name: &str) -> Self {
        Self::of_head_kind(TypeHead::Unresolved(name.to_string()))
    }

    #[must_use]
    pub fn infer(var: tir::InferVarId) -> Self {
        Self::of_head_kind(TypeHead::Infer {
            var,
            name: var.to_string(),
        })
    }

    /// [`Self::builtin`] for a declaration every mangler spells bare (`i32`,
    /// `[]`, `Array`), [`Self::declared`] otherwise.
    #[must_use]
    pub fn of_head(defs: &DefTable, def: DefId) -> Self {
        if is_builtin_shape_decl(defs, def) {
            Self::builtin(defs.name(def))
        } else {
            Self::declared(defs, def)
        }
    }

    /// The same head instantiated with `args`.
    #[must_use]
    pub fn with_args(mut self, args: Vec<FqTypeName>) -> Self {
        self.args = args;
        self
    }

    /// The same name behind one more `&` / `&mut`.
    #[must_use]
    pub fn with_reference(mut self, kind: RefKind) -> Self {
        self.reference.insert(0, kind);
        self
    }

    #[must_use]
    pub fn head(&self) -> &TypeHead {
        &self.head
    }

    /// The same head with its type arguments dropped — the base receiver a
    /// dispatch template is named after.
    #[must_use]
    pub fn head_only(&self) -> Self {
        Self {
            reference: self.reference.clone(),
            head: self.head.clone(),
            args: Vec::new(),
        }
    }

    /// The declaration name of the head: no module, no type arguments. This is
    /// the form an `impl` header writes its target as, so it is what an
    /// impl-header scan compares against.
    #[must_use]
    pub fn decl_name(&self) -> DeclName {
        DeclName::new(self.head.name())
    }

    /// The module that declares this type, or `None` for a builtin or binder.
    #[must_use]
    pub fn module(&self) -> Option<&ModuleSource> {
        self.head.module()
    }

    #[must_use]
    pub fn args(&self) -> &[FqTypeName] {
        &self.args
    }

    #[must_use]
    pub fn references(&self) -> &[RefKind] {
        &self.reference
    }

    /// Whether this name or any argument of it is a type-parameter binder: a
    /// name in some template's own parameter space rather than a type.
    #[must_use]
    pub fn mentions_binder(&self) -> bool {
        self.mentions(&|head| matches!(head, TypeHead::Binder { .. }))
    }

    /// Whether `pred` holds of this name's head or of a head inside it.
    pub fn mentions(&self, pred: &impl Fn(&TypeHead) -> bool) -> bool {
        let head_mentions = pred(&self.head)
            || match &self.head {
                TypeHead::Projection(head) => {
                    head.base.mentions(pred)
                        || head
                            .trait_args
                            .iter()
                            .flatten()
                            .any(|arg| arg.mentions(pred))
                }
                TypeHead::Function { signature, .. } => {
                    signature.iter().any(|ty| ty.mentions(pred))
                }
                _ => false,
            };
        head_mentions || self.args.iter().any(|arg| arg.mentions(pred))
    }

    /// Whether some substitution for either side's binders and projections
    /// makes the two names one type. A binder stands for the rest of a type
    /// after the references written before it. A family's parameter is the
    /// exception: it stands for every argument at once, so it is one type, as
    /// rigid as a declaration.
    #[must_use]
    pub fn unifies_with(&self, other: &FqTypeName) -> bool {
        let open = |name: &FqTypeName| match &name.head {
            TypeHead::Binder { owner, .. } => !owner.as_ref().is_some_and(BinderOwner::is_family),
            TypeHead::Projection(_) => true,
            TypeHead::Declared(_)
            | TypeHead::Shape { .. }
            | TypeHead::Builtin(_)
            | TypeHead::Unresolved(_)
            | TypeHead::Infer { .. }
            | TypeHead::Tuple
            | TypeHead::Function { .. } => false,
        };
        if open(self) {
            return other.reference.starts_with(&self.reference);
        }
        if open(other) {
            return self.reference.starts_with(&other.reference);
        }
        if self.reference != other.reference || self.head != other.head {
            return false;
        }
        // A spread stands for a run of a tuple's elements, so it lines the
        // two up from both ends.
        let spread = |name: &FqTypeName| name.args.iter().position(FqTypeName::is_pack_spread);
        let (spread_side, at, plain) = match (spread(self), spread(other)) {
            (Some(at), _) => (self, at, other),
            (None, Some(at)) => (other, at, self),
            (None, None) => {
                return self.args.len() == other.args.len()
                    && self
                        .args
                        .iter()
                        .zip(&other.args)
                        .all(|(a, b)| a.unifies_with(b));
            }
        };
        let after = spread_side.args.len() - at - 1;
        plain.args.len() >= at + after
            && spread_side.args[..at]
                .iter()
                .zip(&plain.args[..at])
                .all(|(a, b)| a.unifies_with(b))
            && spread_side.args[at + 1..]
                .iter()
                .zip(&plain.args[plain.args.len() - after..])
                .all(|(a, b)| a.unifies_with(b))
    }

    /// What this projects off, `None` for any other shape.
    #[must_use]
    pub fn projected(&self) -> Option<Projected<'_>> {
        match &self.head {
            TypeHead::Projection(head) => Some(Projected {
                base: &head.base,
                assoc: &head.assoc,
                owning_trait: head.owning_trait.as_ref().map(DeclaredHead::def),
                trait_args: head.trait_args.as_deref(),
            }),
            _ => None,
        }
    }

    /// The binder this name is, `None` for a name that is not one.
    #[must_use]
    pub fn binder_name(&self) -> Option<&str> {
        match &self.head {
            TypeHead::Binder { name, .. } => Some(name),
            _ => None,
        }
    }

    /// The mangled spelling embedded in a mangled method name.
    #[must_use]
    pub fn to_mangled(&self) -> String {
        let mut out = String::new();
        for kind in &self.reference {
            push_ref_prefix(&mut out, *kind);
        }
        if let TypeHead::Tuple = self.head {
            let elems: Vec<String> = self.args.iter().map(FqTypeName::to_mangled).collect();
            out.push_str(&mangle_tuple_type(&elems));
            return out;
        }
        if let Some(spelled) =
            self.fn_spelling(FqTypeName::to_mangled, mangle_effect_ref, mangle_fn_type)
        {
            out.push_str(&spelled);
            return out;
        }
        match &self.head {
            TypeHead::Declared(head) => {
                out.push_str(&mangle_declared(head.module(), head.mangled()));
            }
            TypeHead::Shape { module, name } => out.push_str(&mangle_declared(module, name)),
            TypeHead::Builtin(name) | TypeHead::Unresolved(name) | TypeHead::Infer { name, .. } => {
                out.push_str(name);
            }
            TypeHead::Binder { name, owner } => match owner {
                Some(owner) => out.push_str(&format!("{name}#{}", owner.mangled())),
                None => out.push_str(name),
            },
            TypeHead::Projection(head) => {
                out.push_str(&format!("{}::{}", head.base.to_mangled(), head.assoc));
                if let Some(owning_trait) = &head.owning_trait {
                    out.push_str(&format!(
                        "#{}/{}",
                        owning_trait.module(),
                        owning_trait.mangled()
                    ));
                }
                if let Some(trait_args) = head.trait_args.as_ref().filter(|args| !args.is_empty()) {
                    let args: Vec<String> = trait_args.iter().map(FqTypeName::to_mangled).collect();
                    out.push_str(&format!("[{}]", args.join(",")));
                }
            }
            TypeHead::Tuple | TypeHead::Function { .. } => unreachable!("handled above"),
        }
        if !self.args.is_empty() {
            let args: Vec<String> = self.args.iter().map(FqTypeName::to_mangled).collect();
            out.push('<');
            out.push_str(&args.join(","));
            out.push('>');
        }
        out
    }

    /// This name with `at` applied at every position a type stands in, a
    /// position it answers for replaced whole.
    #[must_use]
    pub fn rewrite(&self, at: &impl Fn(&FqTypeName) -> Option<FqTypeName>) -> FqTypeName {
        if let Some((outer, pointee)) = self.split_reference() {
            return pointee.rewrite(at).with_reference(outer);
        }
        match at(self) {
            Some(replacement) => replacement,
            // A spread stands for a run of the tuple's elements, so the tuple
            // it is answered with splices in rather than nesting.
            None if self.head == TypeHead::Tuple => FqTypeName::tuple(
                self.args
                    .iter()
                    .flat_map(|arg| {
                        let rewritten = arg.rewrite(at);
                        if arg.is_pack_spread() && rewritten.is_bare_tuple() {
                            rewritten.args
                        } else {
                            vec![rewritten]
                        }
                    })
                    .collect(),
            ),
            None => self.descend(&|inner| inner.rewrite(at)),
        }
    }

    fn is_bare_tuple(&self) -> bool {
        self.head == TypeHead::Tuple && self.reference.is_empty()
    }

    /// The outermost `&` prefix and what it points at, `None` for a name
    /// carrying no prefix.
    #[must_use]
    pub fn split_reference(&self) -> Option<(RefKind, FqTypeName)> {
        let (outer, inner) = self.reference.split_first()?;
        Some((
            *outer,
            FqTypeName {
                reference: inner.to_vec(),
                head: self.head.clone(),
                args: self.args.clone(),
            },
        ))
    }

    /// This name rebuilt with `at` applied to each name it holds: its type
    /// arguments and a projection's base and trait arguments, never the
    /// head's own spelling.
    fn descend(&self, at: &impl Fn(&FqTypeName) -> FqTypeName) -> FqTypeName {
        let head = match &self.head {
            TypeHead::Projection(projected) => TypeHead::Projection(Box::new(ProjectionHead {
                base: at(&projected.base),
                assoc: projected.assoc.clone(),
                owning_trait: projected.owning_trait.clone(),
                trait_args: projected
                    .trait_args
                    .as_ref()
                    .map(|args| args.iter().map(at).collect()),
            })),
            TypeHead::Function {
                is_mut,
                signature,
                effects,
            } => TypeHead::Function {
                is_mut: *is_mut,
                signature: signature.iter().map(at).collect(),
                effects: effects.clone(),
            },
            head => head.clone(),
        };
        FqTypeName {
            reference: Vec::new(),
            head,
            args: self.args.iter().map(at).collect(),
        }
    }

    /// This name with every function type's row in it, nested ones included,
    /// replaced by what `row` answers for it.
    #[must_use]
    pub fn map_rows(&self, row: &impl Fn(&[tir::EffectRef]) -> Vec<tir::EffectRef>) -> FqTypeName {
        let mut out = self.descend(&|name| name.map_rows(row));
        if let TypeHead::Function { effects, .. } = &mut out.head {
            *effects = row(effects);
            tir::canonicalize_row(effects);
        }
        out.reference.clone_from(&self.reference);
        out
    }

    /// The spelling a diagnostic prints: the type as source writes it, modules
    /// dropped from the head and, recursively, from every type in it.
    #[must_use]
    pub fn to_display(&self) -> String {
        let mut out = String::new();
        for kind in &self.reference {
            push_ref_prefix(&mut out, *kind);
        }
        let args: Vec<String> = self.args.iter().map(FqTypeName::to_display).collect();
        if let TypeHead::Tuple = self.head {
            out.push_str(&display_tuple_type(&args));
            return out;
        }
        if let Some(spelled) = self.fn_spelling(
            FqTypeName::to_display,
            |effect| effect.name().to_string(),
            display_fn_type,
        ) {
            out.push_str(&spelled);
            return out;
        }
        if let TypeHead::Projection(head) = &self.head {
            out.push_str(&assoc_type_projection_name(
                &head.base.to_display(),
                &head.assoc,
            ));
        } else {
            out.push_str(self.head.name());
        }
        out.push_str(&display_type_args(&args));
        out
    }

    /// A function type spelled by `spell`, each type in it by `render` and
    /// each effect by `effect`. `None` for any other head.
    fn fn_spelling(
        &self,
        render: fn(&FqTypeName) -> String,
        effect: fn(&tir::EffectRef) -> String,
        spell: FnTypeSpeller,
    ) -> Option<String> {
        let TypeHead::Function {
            is_mut,
            signature,
            effects,
        } = &self.head
        else {
            return None;
        };
        let (ret, params) = signature
            .split_last()
            .expect("a function type's signature ends in its return");
        let params: Vec<String> = params.iter().map(render).collect();
        let effects: Vec<String> = effects.iter().map(effect).collect();
        Some(spell(
            *is_mut,
            &params,
            &render(ret),
            matches!(ret.head, TypeHead::Function { .. }) && ret.reference.is_empty(),
            &effects,
        ))
    }

    #[must_use]
    pub fn into_string(self) -> String {
        self.to_mangled()
    }
}

impl std::fmt::Display for FqTypeName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_mangled())
    }
}

/// What a projection name projects off; see [`FqTypeName::projected`].
pub struct Projected<'n> {
    /// The type the associated type is projected off.
    pub base: &'n FqTypeName,
    /// The associated type's name.
    pub assoc: &'n str,
    /// The trait declaring it; see [`ProjectionHead`].
    pub owning_trait: Option<DefId>,
    /// The arguments the trait is reached at; see [`ProjectionHead`].
    pub trait_args: Option<&'n [FqTypeName]>,
}

/// The item a binder belongs to: the `impl` block a blanket's receiver binder
/// belongs to, or the generic associated type a family parameter does.
/// Equality and hashing read the item alone, so two blankets of one trait
/// write two template names whatever letter each spells its parameter (#1932).
#[derive(Debug, Clone)]
pub struct BinderOwner {
    id: AstId,
    /// The associated type within the trait at `id`, for a family parameter.
    assoc: Option<String>,
    /// What a mangle embeds: the declaring module plus the node's
    /// *module-local* `AstId` index — never the `AstIdSpace`, which is a
    /// process-global counter and would make mangled names non-deterministic
    /// (see [`mangle_local_item_name`]).
    mangled: String,
}

impl PartialEq for BinderOwner {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.assoc == other.assoc
    }
}

impl Eq for BinderOwner {}

impl std::hash::Hash for BinderOwner {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.assoc.hash(state);
    }
}

impl BinderOwner {
    /// A parameter of the item declared at `id` in `module`.
    fn of_item(module: &ModuleSource, id: AstId) -> Self {
        Self {
            id,
            assoc: None,
            mangled: format!("{module}/{}", id.local()),
        }
    }

    /// Whether this owns a generic associated type's parameter.
    fn is_family(&self) -> bool {
        self.assoc.is_some()
    }

    /// A parameter of the associated type `assoc` of the trait declared at
    /// `id` in `module`.
    fn of_family(module: &ModuleSource, id: AstId, assoc: &str) -> Self {
        Self {
            id,
            assoc: Some(assoc.to_string()),
            mangled: format!("{module}/{}::{assoc}", id.local()),
        }
    }

    /// The spelling a mangle embeds.
    fn mangled(&self) -> &str {
        &self.mangled
    }
}

/// A declaration in a name's head position, with the spellings it renders to.
/// Equality and hashing read the [`crate::defs::DefId`] alone.
#[derive(Debug, Clone)]
pub struct DeclaredHead {
    def: DefId,
    module: ModuleSource,
    /// As source writes it. Diagnostics.
    name: String,
    /// What a mangle embeds: [`Self::name`] with a function-local
    /// declaration's disambiguator applied.
    mangled: String,
}

impl PartialEq for DeclaredHead {
    fn eq(&self, other: &Self) -> bool {
        self.def == other.def
    }
}

impl Eq for DeclaredHead {}

impl std::hash::Hash for DeclaredHead {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.def.hash(state);
    }
}

impl DeclaredHead {
    /// Render `def`. The declaring module and the declared name come off the
    /// table, never from a caller.
    #[must_use]
    pub fn new(defs: &DefTable, def: DefId) -> Self {
        let name = defs.name(def).to_string();
        let mangled = if defs.is_function_local(def) {
            mangle_local_item_name(&name, defs.ast_id(def))
        } else {
            name.clone()
        };
        Self {
            def,
            module: defs.module(def).clone(),
            name,
            mangled,
        }
    }

    /// The spelling a mangle embeds.
    #[must_use]
    pub fn mangled(&self) -> &str {
        &self.mangled
    }

    #[must_use]
    pub fn def(&self) -> DefId {
        self.def
    }

    #[must_use]
    pub fn module(&self) -> &ModuleSource {
        &self.module
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// The trait half of a `Type^Trait::method` mangle. The head is the trait's
/// *declaration* — an identity, never the spelling a use site wrote, which an
/// alias or another module's import can change — or two same-named traits
/// implemented for one type would mangle alike and one impl would overwrite
/// the other. Type arguments are carried already-mangled.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FqTraitName {
    head: TraitHead,
    /// The trait's type arguments, structured — each an identity or a shape, never
    /// a spelling, so an impl header's `Index<K>` and a call site's reach one head
    /// and cannot render a trait segment differently.
    args: Vec<FqTypeName>,
}

/// What an [`FqTraitName`]'s head names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TraitHead {
    /// A trait declaration.
    Declared(DeclaredHead),
    /// A trait position filled by a template's own binder. Not a declaration,
    /// so it has no module and no identity — only the spelling its template
    /// scopes.
    Binder(String),
}

impl TraitHead {
    fn name(&self) -> &str {
        match self {
            Self::Declared(head) => head.name(),
            Self::Binder(name) => name,
        }
    }
}

impl FqTraitName {
    #[must_use]
    pub fn declared(defs: &DefTable, def: DefId) -> Self {
        Self {
            head: TraitHead::Declared(DeclaredHead::new(defs, def)),
            args: Vec::new(),
        }
    }

    /// A trait position filled by a template's own binder rather than a
    /// declaration. It has no module, like [`FqTypeName::binder`].
    #[must_use]
    pub fn binder(name: &str) -> Self {
        Self {
            head: TraitHead::Binder(name.to_string()),
            args: Vec::new(),
        }
    }

    /// The same trait with its type arguments.
    #[must_use]
    pub fn with_args(mut self, args: Vec<FqTypeName>) -> Self {
        self.args = args;
        self
    }

    /// The trait's declaration name: no module, no type arguments. This is what
    /// the by-name decl indices key on.
    #[must_use]
    pub fn base_name(&self) -> &str {
        self.head.name()
    }

    /// The declaring module, or `None` for a binder.
    #[must_use]
    pub fn module(&self) -> Option<&ModuleSource> {
        match &self.head {
            TraitHead::Declared(head) => Some(head.module()),
            TraitHead::Binder(_) => None,
        }
    }

    /// The declaration this names, or `None` for a binder.
    #[must_use]
    pub fn declared_head(&self) -> Option<&DeclaredHead> {
        match &self.head {
            TraitHead::Declared(head) => Some(head),
            TraitHead::Binder(_) => None,
        }
    }

    /// The trait this names, or `None` for a binder. This is the identity —
    /// compare these, never [`Self::base_name`].
    #[must_use]
    pub fn canonical(&self) -> Option<DefId> {
        match &self.head {
            TraitHead::Declared(head) => Some(head.def()),
            TraitHead::Binder(_) => None,
        }
    }

    /// The trait a method call names. Elaboration rejects one reaching no
    /// declaration, so only a bound's binder is `None` for [`Self::canonical`].
    #[must_use]
    pub fn called_decl(&self) -> DefId {
        self.canonical()
            .expect("elaboration rejects a trait reaching no declaration")
    }

    #[must_use]
    pub fn args(&self) -> &[FqTypeName] {
        &self.args
    }

    /// The same trait with every row in its arguments replaced by what `row`
    /// answers for it.
    #[must_use]
    pub fn map_rows(&self, row: &impl Fn(&[tir::EffectRef]) -> Vec<tir::EffectRef>) -> Self {
        let args = self.args.iter().map(|arg| arg.map_rows(row)).collect();
        self.clone().with_args(args)
    }

    /// Whether any type argument still names a binder, which makes this a
    /// question in a template's parameter space that no concrete impl answers.
    #[must_use]
    pub fn args_mention_binder(&self) -> bool {
        self.args.iter().any(FqTypeName::mentions_binder)
    }

    /// This trait with [`FqTypeName::rewrite`] applied to each argument: every
    /// replacement made at once, so one never rewrites another's result.
    #[must_use]
    pub fn rewrite_args(&self, at: &impl Fn(&FqTypeName) -> Option<FqTypeName>) -> Self {
        Self {
            head: self.head.clone(),
            args: self.args.iter().map(|a| a.rewrite(at)).collect(),
        }
    }

    /// The same trait with its type arguments dropped.
    #[must_use]
    pub fn head_only(&self) -> Self {
        Self {
            head: self.head.clone(),
            args: Vec::new(),
        }
    }

    /// The spelling embedded in a mangled method name.
    #[must_use]
    pub fn to_mangled(&self) -> String {
        let head = match &self.head {
            TraitHead::Declared(head) => mangle_declared(head.module(), head.mangled()),
            TraitHead::Binder(name) => name.clone(),
        };
        let args: Vec<String> = self.args.iter().map(FqTypeName::to_mangled).collect();
        mangle_generic_name(&head, &args)
    }

    /// The trait as source writes it: the declaring module dropped. Diagnostics
    /// only.
    #[must_use]
    pub fn to_display(&self) -> String {
        let args: Vec<String> = self.args.iter().map(FqTypeName::to_display).collect();
        display_generic_name(self.head.name(), &args)
    }
}

impl std::fmt::Display for FqTraitName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_mangled())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_function_type_name_spells_as_its_mangle() {
        let inner = FqTypeName::function(false, vec![], FqTypeName::builtin("()"), vec![]);
        let f = FqTypeName::function(
            true,
            vec![FqTypeName::binder("T"), FqTypeName::builtin("i32")],
            inner,
            vec![tir::EffectRef::Param(tir::EffectParam {
                binder: crate::ast::AstId::fresh(),
                name: "E".into(),
            })],
        );
        assert_eq!(f.to_mangled(), "fn mut(T,i32)->(fn()->())with(E)");
        assert_eq!(f.to_display(), "fn mut(T, i32) -> (fn() -> ()) with E");
        assert!(f.mentions_binder());
        assert_eq!(f.head_only(), f);
    }
}
