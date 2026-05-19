//! Type definitions used across the resolver phase.

use std::cell::RefCell;

use crate::hashmap::{IndexMap, IndexSet};

use crate::ast::{self, AstId};
use crate::module_source::ModuleSource;
use crate::tir::TypeId;
use crate::token::Span;

/// Struct field info: module source and field definitions
#[derive(Clone)]
pub(crate) struct StructFieldInfo {
    /// Canonical type name (original declaration name, not import alias).
    pub(super) name: String,
    pub(super) module_source: ModuleSource,
    /// Field definitions: (name, `type_id`, `is_pub`) triples
    pub(super) fields: Vec<(String, TypeId, bool)>,
    /// Parallel array to `fields` holding each field's defining `AstId`.
    /// Used for recording use→def references (e.g. field access → field def).
    pub(super) field_ast_ids: Vec<AstId>,
    /// Default-value expressions, parallel to `fields`.
    /// `Some(expr)` means the field declared `= expr` and may be omitted at
    /// construction; `None` means the field is required.
    pub(super) field_defaults: Vec<Option<ast::Expr>>,
    /// Type parameter bounds: (`param_name`, `trait_bounds`)
    /// E.g., for `struct Sorted<T: Ord>`, this would be `[("T", ["Ord"])]`
    pub(super) type_param_bounds: Vec<(String, Vec<String>)>,
    /// `TypeIds` of the struct's own type parameters in declaration order.
    /// Used by `infer_struct_type_args` to fill phantom type params
    /// (e.g., `D` in `struct DirMap<D, V>` where D doesn't appear in any field).
    pub(super) type_param_type_ids: Vec<TypeId>,
}

/// Variant case info: case name and payload type
#[derive(Clone)]
pub(super) struct VariantCaseData {
    pub(super) name: String,
    /// Payload type for this case. Unit variants have `()` (unit type) payload.
    pub(super) payload: TypeId,
    /// `AstId` of the case declaration (`VariantCase::id`) in the owning module.
    pub(super) ast_id: AstId,
}

/// Variant info: module source, type parameters, and cases
#[derive(Clone)]
pub(crate) struct VariantInfo {
    /// Canonical type name (original declaration name, not import alias).
    pub(super) name: String,
    pub(super) module_source: ModuleSource,
    pub(super) type_params: Vec<String>,
    pub(super) cases: Vec<VariantCaseData>,
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
    /// Canonical type name (original declaration name, not import alias).
    pub(super) name: String,
    pub(super) module_source: ModuleSource,
    pub(super) cases: Vec<EnumCaseData>,
    /// O(1) lookup from case name to discriminant index
    pub(super) case_index: crate::hashmap::IndexMap<String, u32>,
}

impl EnumInfo {
    pub(super) fn new(name: String, module_source: ModuleSource, cases: Vec<EnumCaseData>) -> Self {
        let case_index = cases.iter().map(|c| (c.name.clone(), c.index)).collect();
        Self {
            name,
            module_source,
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
    pub(super) members: Vec<FlagsMemberData>,
    /// Module source that owns this flags declaration. Used to build `SymbolKey`
    /// entries for use->def references pointing at individual members.
    pub(super) module_source: ModuleSource,
}

/// Resource info: module source and method names
/// Note: This infrastructure was added for resource static methods but isn't fully used yet.
/// Keep it for when wasi:sockets registration is re-enabled.
#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct ResourceInfo {
    /// Canonical type name (original declaration name, not import alias).
    pub(super) name: String,
    pub(super) module_source: ModuleSource,
    /// Method names defined on this resource (both static and instance)
    pub(super) methods: Vec<String>,
}

/// Generic newtype definition: `type Foo<T> = Bar<T>`
#[derive(Clone)]
pub(crate) struct GenericNewtypeInfo {
    pub(super) module_source: ModuleSource,
    pub(super) type_params: Vec<String>,
    pub(super) base_type_ast: ast::Type,
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

    /// Unknown type name
    UnknownType { name: String, span: Span },

    /// Unknown function
    UnknownFunction { name: String, span: Span },

    /// Unknown variable
    UnknownIdentifier { name: String, span: Span },

    /// Field not found on struct
    FieldNotFound {
        struct_name: String,
        field_name: String,
        span: Span,
    },

    /// Wrong number of arguments
    ArgumentCountMismatch {
        expected: usize,
        found: usize,
        span: Span,
    },

    /// Expression in callee position whose type is not a function.
    CalleeNotCallable { type_name: String, span: Span },

    /// Invalid numeric literal
    InvalidLiteral { message: String, span: Span },

    /// Feature not yet implemented
    NotYetImplemented { feature: String, span: Span },

    /// Invalid assignment target (not a valid l-value)
    CannotAssign { message: String, span: Span },

    /// Trait bound not satisfied
    TraitBoundNotSatisfied {
        type_name: String,
        trait_name: String,
        param_name: String,
        span: Span,
    },

    /// Invalid pattern in context
    InvalidPattern { message: String, span: Span },

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
    DuplicateField { name: String, span: Span },

    /// Function/method/closure with return type but no return statement
    MissingReturn { return_type: String, span: Span },

    /// Orphan rule violation: impl of a foreign trait for a foreign type
    OrphanViolation {
        trait_name: String,
        self_type_name: String,
        span: Span,
    },

    /// Invalid stores declaration
    InvalidStores { message: String, span: Span },

    /// Private field access from outside the declaring module
    PrivateFieldAccess {
        struct_name: String,
        field_name: String,
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
    InvalidQuestionMark { message: String, span: Span },

    /// Type does not implement required trait in usage context
    MissingTraitImpl {
        type_name: String,
        trait_name: String,
        span: Span,
    },

    /// Pattern expects different type kind (tuple, struct, variant, enum)
    PatternTypeMismatch {
        expected: String,
        found: String,
        span: Span,
    },

    /// Trait impl cannot re-specify a parameter default (defaults belong to
    /// the trait declaration).
    DefaultInTraitImpl {
        method: String,
        param: String,
        span: Span,
    },

    /// Closures cannot declare default parameter values. Closures erase
    /// defaults when assigned to a `fn(...)` type, so allowing them would
    /// be misleading — the default only ever applies to direct calls.
    DefaultInClosure { param: String, span: Span },

    /// Calling a `fn mut` closure requires its binding to be `let mut` (for
    /// locals) or `mut name:` (for parameters). Mirrors Rust's `FnMut` rule.
    ClosureMutBindingRequired { name: String, span: Span },

    /// Closures cannot cross the Component Model boundary. Triggered when an
    /// `export` (or imported) function has a closure-typed parameter, return
    /// type, or buried closure inside a container type.
    ClosureAtCmBoundary {
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

    /// `resume` expression appeared outside an effect handler method body.
    /// `resume value` is only valid inside the body of a method belonging
    /// to an `impl Effect for Type` block (see WEP 2026-04-11).
    ResumeOutsideHandler { span: Span },

    /// `with E => h do { ... }` clause where the handler value's type
    /// does not implement effect `E`.
    HandlerEffectNotImplemented {
        type_name: String,
        interface_name: String,
        span: Span,
    },

    /// Bundled-handler form `with &mut h do { ... }` where the handler value's
    /// underlying type does not implement any effect. There is nothing for
    /// `with h do` to install in this case — the user almost certainly meant
    /// to write `with E => h do` instead.
    BundledHandlerImplementsNoEffect { type_name: String, span: Span },

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
    NotAnEffect { name: String, span: Span },

    /// `with E => h do` where `E` is a generic effect parameter
    /// (`<effect E>`). Generic effect parameters are propagation-only:
    /// the compiler does not know `E`'s operation list at effect-check
    /// time, so it cannot generate dispatch infrastructure. See WEP
    /// 2026-01-27 § Generic Effect Parameters Are Propagation-Only.
    GenericEffectParamNotInstallable { name: String, span: Span },

    /// `#[compiler_item("...")]` attribute that failed validation —
    /// unknown name, kind mismatch (e.g. `#[compiler_item("option")]`
    /// on a struct), or used outside a `core::*` module. The
    /// `message` carries the specific problem.
    CompilerItemAttr { message: String, span: Span },

    /// A bare generic function name was used as a value with no expected
    /// `fn(...)` type to drive inference. The function type depends on
    /// type arguments that have not been supplied. The fix is to either
    /// pin them with turbofish (`name::<T, ...>`) or wrap the call in a
    /// closure (`|x| name(x)`) so the inner call site provides the
    /// inference context.
    BareGenericFunctionRef { name: String, span: Span },

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
    /// an expected `fn(...)` type, but the function's parameter / return
    /// shape does not match the expected signature closely enough to
    /// infer the type arguments. Only positional one-to-one inference is
    /// attempted today (see WEP TBD); the workaround is to write a
    /// turbofish.
    GenericFunctionRefInferenceFailed {
        name: String,
        expected_type: String,
        span: Span,
    },
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeError::TypeMismatch {
                expected,
                found,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: type mismatch: expected '{}', found '{}'",
                    span.line, span.column, expected, found
                )
            }
            TypeError::UnknownType { name, span } => {
                write!(f, "{}:{}: unknown type '{}'", span.line, span.column, name)
            }
            TypeError::UnknownFunction { name, span } => {
                write!(
                    f,
                    "{}:{}: unknown function '{}'",
                    span.line, span.column, name
                )
            }
            TypeError::UnknownIdentifier { name, span } => {
                write!(
                    f,
                    "{}:{}: unknown identifier '{}'",
                    span.line, span.column, name
                )
            }
            TypeError::FieldNotFound {
                struct_name,
                field_name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: field '{}' not found on struct '{}'",
                    span.line, span.column, field_name, struct_name
                )
            }
            TypeError::ArgumentCountMismatch {
                expected,
                found,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: expected {} arguments, found {}",
                    span.line, span.column, expected, found
                )
            }
            TypeError::CalleeNotCallable { type_name, span } => {
                write!(
                    f,
                    "{}:{}: expression is not callable: type '{}' is not a function",
                    span.line, span.column, type_name
                )
            }
            TypeError::InvalidLiteral { message, span } => {
                write!(f, "{}:{}: {}", span.line, span.column, message)
            }
            TypeError::NotYetImplemented { feature, span } => {
                write!(
                    f,
                    "{}:{}: {} is not yet implemented",
                    span.line, span.column, feature
                )
            }
            TypeError::CannotAssign { message, span } => {
                write!(
                    f,
                    "{}:{}: cannot assign: {}",
                    span.line, span.column, message
                )
            }
            TypeError::TraitBoundNotSatisfied {
                type_name,
                trait_name,
                param_name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: type '{}' does not implement trait '{}' required by bound on '{}'",
                    span.line, span.column, type_name, trait_name, param_name
                )
            }
            TypeError::InvalidPattern { message, span } => {
                write!(
                    f,
                    "{}:{}: invalid pattern: {}",
                    span.line, span.column, message
                )
            }
            TypeError::InvalidCast {
                from,
                to,
                hint,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: cannot cast '{}' to '{}': {}",
                    span.line, span.column, from, to, hint
                )
            }
            TypeError::MissingField {
                struct_name,
                field_name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: missing field '{}' in struct literal '{}'",
                    span.line, span.column, field_name, struct_name
                )
            }
            TypeError::ExtraField {
                struct_name,
                field_name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: struct '{}' has no field '{}'",
                    span.line, span.column, struct_name, field_name
                )
            }
            TypeError::DuplicateField { name, span } => {
                write!(
                    f,
                    "{}:{}: duplicate field '{}'",
                    span.line, span.column, name
                )
            }
            TypeError::MissingReturn { return_type, span } => {
                write!(
                    f,
                    "{}:{}: function with return type '{}' must use explicit `return`",
                    span.line, span.column, return_type
                )
            }
            TypeError::OrphanViolation {
                trait_name,
                self_type_name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: orphan rule violation: cannot implement foreign trait `{}` for foreign type `{}`",
                    span.line, span.column, trait_name, self_type_name
                )
            }
            TypeError::InvalidStores { message, span } => {
                write!(f, "{}:{}: {}", span.line, span.column, message)
            }
            TypeError::PrivateFieldAccess {
                struct_name,
                field_name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: field `{}` of struct `{}` is private",
                    span.line, span.column, field_name, struct_name
                )
            }
            TypeError::MethodNotFound {
                type_name,
                method_name,
                hint,
                span,
            } => {
                if hint.is_empty() {
                    write!(
                        f,
                        "{}:{}: no method '{}' found on type '{}'",
                        span.line, span.column, method_name, type_name
                    )
                } else {
                    write!(
                        f,
                        "{}:{}: no method '{}' found on type '{}'; {}",
                        span.line, span.column, method_name, type_name, hint
                    )
                }
            }
            TypeError::InvalidQuestionMark { message, span } => {
                write!(f, "{}:{}: {}", span.line, span.column, message)
            }
            TypeError::MissingTraitImpl {
                type_name,
                trait_name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: type '{}' does not implement {}",
                    span.line, span.column, type_name, trait_name
                )
            }
            TypeError::PatternTypeMismatch {
                expected,
                found,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: pattern mismatch: expected '{}', found '{}'",
                    span.line, span.column, expected, found
                )
            }
            TypeError::DefaultInTraitImpl {
                method,
                param,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: default value for parameter '{}' in method '{}' is not allowed; defaults belong to the trait declaration",
                    span.line, span.column, param, method
                )
            }
            TypeError::DefaultInClosure { param, span } => {
                write!(
                    f,
                    "{}:{}: default value for parameter '{}' is not allowed in closure; closures erase defaults when assigned to a function type",
                    span.line, span.column, param
                )
            }
            TypeError::ClosureMutBindingRequired { name, span } => {
                write!(
                    f,
                    "{}:{}: cannot call `fn mut` closure '{}' through a non-`mut` binding; use `let mut {}`",
                    span.line, span.column, name, name
                )
            }
            TypeError::ClosureAtCmBoundary {
                function,
                position,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: closure type in {} of `{}` is not allowed: closures cannot cross the Component Model boundary",
                    span.line, span.column, position, function
                )
            }
            TypeError::DefaultInExportFn {
                function,
                param,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: default value for parameter '{}' in export fn '{}' is not allowed; the Component Model ABI requires every parameter at the boundary",
                    span.line, span.column, param, function
                )
            }
            TypeError::ResumeOutsideHandler { span } => {
                write!(
                    f,
                    "{}:{}: `resume` is only valid inside an effect handler method body",
                    span.line, span.column
                )
            }
            TypeError::HandlerEffectNotImplemented {
                type_name,
                interface_name,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: handler value of type '{}' does not implement interface '{}'",
                    span.line, span.column, type_name, interface_name
                )
            }
            TypeError::BundledHandlerImplementsNoEffect { type_name, span } => {
                write!(
                    f,
                    "{}:{}: handler value of type '{}' does not implement any effect; use `with E => h do` instead",
                    span.line, span.column, type_name
                )
            }
            TypeError::BundledHandlerUnsupportedHandlerType {
                type_name,
                type_kind,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: bundled effect handler `with h do` is not supported for handler type '{}' ({}); use the explicit form `with E => h do` instead",
                    span.line, span.column, type_name, type_kind
                )
            }
            TypeError::NotAnEffect { name, span } => {
                write!(
                    f,
                    "{}:{}: '{}' is not an effect or resource; only effect and resource names are valid in `with E => h do` clauses",
                    span.line, span.column, name
                )
            }
            TypeError::GenericEffectParamNotInstallable { name, span } => {
                write!(
                    f,
                    "{}:{}: cannot install a handler for generic effect parameter '{}'; abstract effect parameters are propagation-only — use a concrete effect name in `with`",
                    span.line, span.column, name
                )
            }
            TypeError::CompilerItemAttr { message, span } => {
                write!(f, "{}:{}: {}", span.line, span.column, message)
            }
            TypeError::BareGenericFunctionRef { name, span } => {
                write!(
                    f,
                    "{}:{}: cannot reference generic function '{}' bare; supply type arguments via turbofish (e.g., `{}::<…>`) or wrap in a closure (e.g., `|x| {}(x)`)",
                    span.line, span.column, name, name, name
                )
            }
            TypeError::GenericFunctionRefArgCountMismatch {
                name,
                expected,
                found,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: wrong number of type arguments for generic function '{}': expected {}, found {}",
                    span.line, span.column, name, expected, found
                )
            }
            TypeError::GenericFunctionRefInferenceFailed {
                name,
                expected_type,
                span,
            } => {
                write!(
                    f,
                    "{}:{}: cannot infer type arguments for generic function '{}' from expected type '{}'; supply them via turbofish (e.g., `{}::<…>`)",
                    span.line, span.column, name, expected_type, name
                )
            }
        }
    }
}

impl std::error::Error for TypeError {}

impl From<TypeError> for crate::compiler_host::Diagnostic {
    fn from(e: TypeError) -> Self {
        use crate::compiler_host::{Code, DiagnosticSpan, Severity};
        let (code, message, span) = match &e {
            TypeError::TypeMismatch {
                expected,
                found,
                span,
            } => (
                Code::TypeMismatch,
                format!("type mismatch: expected '{expected}', found '{found}'"),
                *span,
            ),
            TypeError::UnknownType { name, span } => {
                (Code::UnknownType, format!("unknown type '{name}'"), *span)
            }
            TypeError::UnknownFunction { name, span } => (
                Code::UndefinedVariable,
                format!("unknown function '{name}'"),
                *span,
            ),
            TypeError::UnknownIdentifier { name, span } => (
                Code::UndefinedVariable,
                format!("unknown identifier '{name}'"),
                *span,
            ),
            TypeError::FieldNotFound {
                struct_name,
                field_name,
                span,
            } => (
                Code::TypeMismatch,
                format!("field '{field_name}' not found on struct '{struct_name}'"),
                *span,
            ),
            TypeError::ArgumentCountMismatch {
                expected,
                found,
                span,
            } => (
                Code::TypeMismatch,
                format!("expected {expected} arguments, found {found}"),
                *span,
            ),
            TypeError::CalleeNotCallable { type_name, span } => (
                Code::TypeMismatch,
                format!("expression is not callable: type '{type_name}' is not a function"),
                *span,
            ),
            TypeError::InvalidLiteral { message, span } => {
                (Code::InvalidSyntax, message.clone(), *span)
            }
            TypeError::NotYetImplemented { feature, span } => (
                Code::UnsupportedFeature,
                format!("{feature} is not yet implemented"),
                *span,
            ),
            TypeError::CannotAssign { message, span } => (
                Code::ImmutableAssignment,
                format!("cannot assign: {message}"),
                *span,
            ),
            TypeError::TraitBoundNotSatisfied {
                type_name,
                trait_name,
                param_name,
                span,
            } => (
                Code::TypeMismatch,
                format!(
                    "type '{type_name}' does not implement trait '{trait_name}' required by bound on '{param_name}'"
                ),
                *span,
            ),
            TypeError::InvalidPattern { message, span } => (
                Code::InvalidSyntax,
                format!("invalid pattern: {message}"),
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
                Code::TypeMismatch,
                format!("missing field '{field_name}' in struct literal '{struct_name}'"),
                *span,
            ),
            TypeError::ExtraField {
                struct_name,
                field_name,
                span,
            } => (
                Code::TypeMismatch,
                format!("struct '{struct_name}' has no field '{field_name}'"),
                *span,
            ),
            TypeError::DuplicateField { name, span } => (
                Code::DuplicateDefinition,
                format!("duplicate field '{name}' in struct literal"),
                *span,
            ),
            TypeError::MissingReturn { return_type, span } => (
                Code::TypeMismatch,
                format!("function with return type '{return_type}' must use explicit `return`"),
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
            TypeError::InvalidStores { message, span } => {
                (Code::InvalidSyntax, message.clone(), *span)
            }
            TypeError::PrivateFieldAccess {
                struct_name,
                field_name,
                span,
            } => (
                Code::ImmutableAssignment,
                format!("field `{field_name}` of struct `{struct_name}` is private"),
                *span,
            ),
            TypeError::MethodNotFound {
                type_name,
                method_name,
                hint,
                span,
            } => (
                Code::UndefinedVariable,
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
                Code::TypeMismatch,
                format!("type '{type_name}' does not implement {trait_name}"),
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
            TypeError::DefaultInTraitImpl {
                method,
                param,
                span,
            } => (
                Code::TypeMismatch,
                format!(
                    "default value for parameter '{param}' in method '{method}' is not allowed; defaults belong to the trait declaration"
                ),
                *span,
            ),
            TypeError::DefaultInClosure { param, span } => (
                Code::TypeMismatch,
                format!(
                    "default value for parameter '{param}' is not allowed in closure; closures erase defaults when assigned to a function type"
                ),
                *span,
            ),
            TypeError::ClosureMutBindingRequired { name, span } => (
                Code::TypeMismatch,
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
                Code::TypeMismatch,
                format!(
                    "closure type in {position} of `{function}` is not allowed: closures cannot cross the Component Model boundary"
                ),
                *span,
            ),
            TypeError::DefaultInExportFn {
                function,
                param,
                span,
            } => (
                Code::TypeMismatch,
                format!(
                    "default value for parameter '{param}' in export fn '{function}' is not allowed; the Component Model ABI requires every parameter at the boundary"
                ),
                *span,
            ),
            TypeError::ResumeOutsideHandler { span } => (
                Code::UnsupportedFeature,
                "`resume` is only valid inside an effect handler method body".to_string(),
                *span,
            ),
            TypeError::HandlerEffectNotImplemented {
                type_name,
                interface_name,
                span,
            } => (
                Code::TypeMismatch,
                format!(
                    "handler value of type '{type_name}' does not implement interface '{interface_name}'"
                ),
                *span,
            ),
            TypeError::BundledHandlerImplementsNoEffect { type_name, span } => (
                Code::TypeMismatch,
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
                    "'{name}' is not an effect; only effect names are valid in `with E => h do` clauses"
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
            TypeError::BareGenericFunctionRef { name, span } => (
                Code::TypeMismatch,
                format!(
                    "cannot reference generic function '{name}' bare; supply type arguments via turbofish (e.g., `{name}::<…>`) or wrap in a closure (e.g., `|x| {name}(x)`)"
                ),
                *span,
            ),
            TypeError::GenericFunctionRefArgCountMismatch {
                name,
                expected,
                found,
                span,
            } => (
                Code::TypeMismatch,
                format!(
                    "wrong number of type arguments for generic function '{name}': expected {expected}, found {found}"
                ),
                *span,
            ),
            TypeError::GenericFunctionRefInferenceFailed {
                name,
                expected_type,
                span,
            } => (
                Code::TypeMismatch,
                format!(
                    "cannot infer type arguments for generic function '{name}' from expected type '{expected_type}'; supply them via turbofish (e.g., `{name}::<…>`)"
                ),
                *span,
            ),
        };
        crate::compiler_host::Diagnostic {
            severity: Severity::Error,
            code,
            message,
            span: Some(DiagnosticSpan::from_span(&span, None)),
        }
    }
}

/// Local variable information during resolution
#[derive(Debug, Clone)]
pub(super) struct LocalVar {
    #[allow(dead_code)] // For debugging
    pub(super) name: String,
    pub(super) type_id: TypeId,
    pub(super) index: u32,
    #[allow(dead_code)] // For future mutability checking
    pub(super) is_mut: bool,
    /// `AstId` of the node that introduced this binding (pattern, parameter,
    /// closure parameter). `None` for resolver-synthesized temporaries whose
    /// names cannot be referenced from source (e.g., `__qm_v`, `__b`).
    ///
    /// Used by [`Resolver`] to record `use → def` edges when an `IdentExpr`
    /// resolves to this local, so that LSP can translate a cursor position
    /// into the defining [`SymbolKey`].
    pub(super) defining_ast_id: Option<AstId>,
}

/// Method lookup result including return type, self parameter kind, and parameter types
#[derive(Debug, Clone)]
pub(super) struct MethodInfo {
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
    /// If this method was inherited from a newtype's base type, the base type ID
    /// Used for method signature substitution
    pub(super) inherited_from_base: Option<TypeId>,
    /// CM canonical name from `#[cm("...")]` on resource methods.
    pub(super) cm_name: Option<String>,
    /// True when the method was found on a reference type impl (e.g., `impl Trait for &T`).
    /// The receiver needs an additional auto-ref for `&self` methods (Self is &T, so &self is &&T).
    pub(super) is_ref_impl: bool,
    /// Method-level type parameter `TypeId`s in declaration order (excluding effect params).
    ///
    /// Populated only by lookups that also set up method-level type params in their
    /// resolution scope — currently [`Resolver::find_method_in_trait_bounds`] for
    /// `T::method()` style calls through a type parameter bound. Other producers
    /// leave it empty because their call sites have no method-level inference to
    /// perform (either the method is non-generic, or its type args come from a
    /// separate method-AST lookup such as [`Resolver::infer_method_type_args`]).
    pub(super) method_type_param_ids: Vec<TypeId>,
}

/// Labeled block expression target for tracking break types
#[derive(Debug, Clone)]
pub(super) struct LabeledBlockTarget {
    /// The label name
    pub(super) label: String,
    /// Types collected from `break label: expr;` statements
    pub(super) break_types: Vec<TypeId>,
}

/// Function context during resolution with scope tracking
pub(super) struct FunctionContext {
    /// Stack of scopes (each scope maps name -> `LocalVar`)
    pub(super) scopes: Vec<IndexMap<String, LocalVar>>,
    /// Next local index (Wasm locals are function-wide)
    pub(super) next_local: u32,
    /// Return type of the function (unit for async fns, since they don't Wasm-return a value)
    #[allow(dead_code)] // For future return type checking
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
    pub(super) locals: Vec<crate::tir::TirLocal>,
    /// Local indices that have their address taken (&x or &mut x)
    pub(super) address_taken_locals: IndexSet<u32>,
    /// Outer context locals for closure capture detection (name -> `LocalVar` snapshot)
    /// Only set for closure contexts
    pub(super) outer_locals: IndexMap<String, LocalVar>,
    /// Captured variables detected during resolution (name -> capture index)
    /// Only used for closure contexts
    pub(super) captured_vars: IndexMap<String, u32>,
    /// Stack of labeled block expression targets for tracking break types
    pub(super) labeled_block_targets: Vec<LabeledBlockTarget>,
    /// Stack of all active labels (from labeled blocks and labeled block expressions)
    pub(super) active_labels: Vec<String>,
    /// Current function name for `#function` compile-time literal
    pub(super) function_name: String,
    /// Deref overrides for mutable closures: maps original var name -> (ref var name, inner type)
    /// When a variable is in this map, lookups return `*__ref_name` instead of the value.
    pub(super) deref_overrides: IndexMap<String, (String, TypeId)>,
    /// Box types for outer address-taken locals: maps outer var name -> `&mut T` type.
    /// When capturing such a variable, use `DerefCapture` to read through the box.
    pub(super) outer_box_types: IndexMap<String, TypeId>,
    /// Per-local closure parameter defaults for `let f = |...| ...` bindings.
    /// Keyed by local variable name; stores `(param_name, default_expr)` in declaration order.
    pub(super) closure_defaults: IndexMap<String, Vec<(String, Option<crate::ast::Expr>)>>,
    /// True when this context represents the body of a method inside an
    /// `impl Effect for Type` block, i.e. an effect handler operation.
    /// `resume value` is only valid in such contexts.
    pub(super) in_handler_method: bool,
}

impl FunctionContext {
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
            labeled_block_targets: Vec::new(),
            active_labels: Vec::new(),
            function_name,
            deref_overrides: IndexMap::default(),
            outer_box_types: IndexMap::default(),
            closure_defaults: IndexMap::default(),
            in_handler_method: false,
        }
    }

    /// Create a closure context with outer scope access for capture detection.
    ///
    /// `type_table` is used to compute `&mut T` types for address-taken outer locals,
    /// enabling correct `DerefCapture` behaviour when those locals are captured.
    pub(super) fn new_closure(
        return_type: TypeId,
        outer_ctx: &FunctionContext,
        type_table: &RefCell<crate::tir::TypeTable>,
    ) -> Self {
        // Snapshot all locals from outer context
        let mut outer_locals = IndexMap::default();
        for scope in &outer_ctx.scopes {
            for (name, local) in scope {
                outer_locals.insert(name.clone(), local.clone());
            }
        }

        // Compute box types for address-taken outer locals
        let mut outer_box_types = IndexMap::default();
        for scope in &outer_ctx.scopes {
            for (name, local) in scope {
                if outer_ctx.address_taken_locals.contains(&local.index) {
                    let ref_type = type_table.borrow_mut().make_mut_ref(local.type_id);
                    outer_box_types.insert(name.clone(), ref_type);
                }
            }
        }

        // Closure function name is parent::{closure}
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
        }
    }

    /// Enter a new scope (for blocks, if/while/for/loop bodies)
    pub(super) fn enter_scope(&mut self) {
        self.scopes.push(IndexMap::default());
    }

    /// Exit the current scope
    pub(super) fn exit_scope(&mut self) {
        self.scopes.pop();
    }

    /// Add a local variable to the current scope.
    ///
    /// `defining_ast_id` identifies the source AST node that introduced this
    /// binding and is used by the resolver to record `use → def` edges. Pass
    /// `Some(id)` for user-visible bindings (let patterns, parameters, closure
    /// parameters); pass `None` for resolver-synthesized temporaries whose
    /// names cannot appear in source (e.g., `__qm_v`, `__b`).
    pub(super) fn add_local(
        &mut self,
        name: String,
        type_id: TypeId,
        is_mut: bool,
        defining_ast_id: Option<AstId>,
    ) -> u32 {
        let index = self.next_local;
        self.next_local += 1;
        self.locals.push(crate::tir::TirLocal {
            name: name.clone(),
            type_id,
            is_mut,
        });

        let scope = self.scopes.last_mut().unwrap();
        scope.insert(
            name.clone(),
            LocalVar {
                name,
                type_id,
                index,
                is_mut,
                defining_ast_id,
            },
        );
        index
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

        // Check deref overrides (for mutable closures: `count` -> `*__ref_count`)
        if let Some((ref_name, inner_type_id)) = self.deref_overrides.get(name).cloned()
            && let Some(ref_local) = self.outer_locals.get(&ref_name).cloned()
        {
            let outer_defining_ast_id = self.outer_locals.get(name).and_then(|l| l.defining_ast_id);
            let capture_index = if let Some(&idx) = self.captured_vars.get(&ref_name) {
                idx
            } else {
                let idx = self.captured_vars.len() as u32;
                self.captured_vars.insert(ref_name.clone(), idx);
                idx
            };
            return Some(VarRef::DerefCapture {
                index: capture_index,
                ref_type_id: ref_local.type_id,
                inner_type_id,
                defining_ast_id: outer_defining_ast_id,
            });
        }

        // Check outer context (for closures)
        if let Some(outer_local) = self.outer_locals.get(name) {
            let inner_type_id = outer_local.type_id;
            let outer_defining_ast_id = outer_local.defining_ast_id;

            // If this outer local has been address-taken (boxed), capture via DerefCapture
            if let Some(&ref_type_id) = self.outer_box_types.get(name) {
                let capture_index = if let Some(&idx) = self.captured_vars.get(name) {
                    idx
                } else {
                    let idx = self.captured_vars.len() as u32;
                    self.captured_vars.insert(name.to_string(), idx);
                    idx
                };
                return Some(VarRef::DerefCapture {
                    index: capture_index,
                    ref_type_id,
                    inner_type_id,
                    defining_ast_id: outer_defining_ast_id,
                });
            }

            // Check if we already captured this variable
            if let Some(&capture_index) = self.captured_vars.get(name) {
                return Some(VarRef::Capture {
                    index: capture_index,
                    type_id: inner_type_id,
                    defining_ast_id: outer_defining_ast_id,
                });
            }

            // Record a new capture
            let capture_index = self.captured_vars.len() as u32;
            self.captured_vars.insert(name.to_string(), capture_index);

            return Some(VarRef::Capture {
                index: capture_index,
                type_id: inner_type_id,
                defining_ast_id: outer_defining_ast_id,
            });
        }

        None
    }

    /// Get the list of captures for building `TirCapture` entries.
    /// For address-taken outer locals, the `LocalVar.type_id` is the box type (`&mut T`).
    pub(super) fn get_captures(&self) -> Vec<(String, u32, LocalVar)> {
        let mut captures: Vec<_> = self
            .captured_vars
            .iter()
            .filter_map(|(name, &index)| {
                self.outer_locals.get(name).map(|local| {
                    // Use box type if the outer variable has its address taken
                    let effective_type_id = self
                        .outer_box_types
                        .get(name)
                        .copied()
                        .unwrap_or(local.type_id);
                    let effective_local = LocalVar {
                        name: local.name.clone(),
                        type_id: effective_type_id,
                        index: local.index,
                        is_mut: local.is_mut,
                        defining_ast_id: local.defining_ast_id,
                    };
                    (name.clone(), index, effective_local)
                })
            })
            .collect();
        // Sort by capture index for consistent ordering
        captures.sort_by_key(|(_, index, _)| *index);
        captures
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
    /// Captured by mutable reference: `*self.__capture_N` (dereferenced)
    DerefCapture {
        index: u32,
        ref_type_id: TypeId,
        inner_type_id: TypeId,
        defining_ast_id: Option<AstId>,
    },
}

/// Result of finding a trait method for a type via `find_trait_method_for_type`.
pub(super) struct TraitMethodMatch {
    pub(super) trait_name: String,
    pub(super) method_info: MethodInfo,
    pub(super) impl_module_source: ModuleSource,
    /// For blanket impl matches (e.g., `impl<I: Iterator> IntoIterator for I`),
    /// this holds the type parameter name (e.g., `"I"`). `None` for normal impls.
    pub(super) blanket_type_param: Option<String>,
    /// The struct name that actually has the trait impl (may differ from the
    /// receiver's struct name when the impl was found through the newtype chain).
    pub(super) impl_struct_name: String,
    /// True for blanket ref impls like `impl<T: Inspect> Inspect for &T` where
    /// the inner type is a type parameter. False for specific ref impls like
    /// `impl IntoIterator for &Array<T>` where the inner type is a concrete generic.
    #[allow(dead_code)] // Used when ref-type impl priority is enabled
    pub(super) is_blanket_ref_impl: bool,
}

/// Read-only view that resolves a type name from a given module's perspective
/// without cloning per-module flat maps.
///
/// Three-layer precedence (highest first):
///   1. Local additions discovered during resolution (anonymous structs, in-progress
///      newtypes/enums declared in the current module's body).
///   2. The current module's own definitions.
///   3. Imports of the current module (with `use { Foo as Bar }` aliasing).
///   4. Any module that defines the name (legacy fallback for prelude-style visibility).
///
/// Constructed cheaply at each call site from the `Resolver`'s context. All
/// fields are borrowed; no heap allocation.
pub(crate) struct TypeLookup<'a> {
    pub(crate) current_module_source: &'a ModuleSource,
    pub(crate) imported_type_sources: &'a IndexMap<String, ModuleSource>,
    pub(crate) import_original_names: &'a IndexMap<String, String>,
    pub(crate) all_newtypes: &'a IndexMap<ModuleSource, IndexMap<String, TypeId>>,
    pub(crate) all_struct_fields: &'a IndexMap<ModuleSource, IndexMap<String, StructFieldInfo>>,
    pub(crate) all_variant_cases: &'a IndexMap<ModuleSource, IndexMap<String, VariantInfo>>,
    pub(crate) all_enum_cases: &'a IndexMap<ModuleSource, IndexMap<String, EnumInfo>>,
    pub(crate) all_flags_cases: &'a IndexMap<ModuleSource, IndexMap<String, FlagsInfo>>,
    pub(crate) all_resource_types: &'a IndexMap<ModuleSource, IndexMap<String, ResourceInfo>>,
    pub(crate) all_generic_newtypes:
        &'a IndexMap<ModuleSource, IndexMap<String, GenericNewtypeInfo>>,
    pub(crate) local_struct_fields: &'a IndexMap<String, StructFieldInfo>,
    pub(crate) local_newtypes: &'a IndexMap<String, TypeId>,
    pub(crate) local_enum_cases: &'a IndexMap<String, EnumInfo>,
    pub(crate) local_flags_cases: &'a IndexMap<String, FlagsInfo>,
    pub(crate) local_generic_newtypes: &'a IndexMap<String, GenericNewtypeInfo>,
    pub(crate) local_variant_cases: &'a IndexMap<String, VariantInfo>,
}

impl<'a> TypeLookup<'a> {
    /// Resolve `name` against `all_per_module` using the (local → current →
    /// imports → any) precedence. Borrows have lifetime `'a`, so the caller
    /// can keep the returned reference alive across `&self` re-borrows.
    fn lookup_ref<V>(
        &self,
        name: &str,
        local: Option<&'a IndexMap<String, V>>,
        all_per_module: &'a IndexMap<ModuleSource, IndexMap<String, V>>,
    ) -> Option<&'a V> {
        if let Some(local) = local
            && let Some(v) = local.get(name)
        {
            return Some(v);
        }
        if let Some(v) = all_per_module
            .get(self.current_module_source)
            .and_then(|m| m.get(name))
        {
            return Some(v);
        }
        if let Some(src) = self.imported_type_sources.get(name) {
            let canonical = self
                .import_original_names
                .get(name)
                .map(String::as_str)
                .unwrap_or(name);
            if let Some(v) = all_per_module.get(src).and_then(|m| m.get(canonical)) {
                return Some(v);
            }
            if let ModuleSource::Wasi { interface } = src {
                let prefix = format!("{interface}/");
                for (s, m) in all_per_module {
                    if let ModuleSource::Wasi { interface: sub } = s
                        && sub.starts_with(&prefix)
                        && let Some(v) = m.get(canonical)
                    {
                        return Some(v);
                    }
                }
            }
        }
        for m in all_per_module.values() {
            if let Some(v) = m.get(name) {
                return Some(v);
            }
        }
        None
    }

    pub(super) fn struct_fields(&self, name: &str) -> Option<&'a StructFieldInfo> {
        self.lookup_ref(name, Some(self.local_struct_fields), self.all_struct_fields)
    }

    pub(super) fn variant_case(&self, name: &str) -> Option<&'a VariantInfo> {
        self.lookup_ref(name, Some(self.local_variant_cases), self.all_variant_cases)
    }

    pub(super) fn enum_case(&self, name: &str) -> Option<&'a EnumInfo> {
        self.lookup_ref(name, Some(self.local_enum_cases), self.all_enum_cases)
    }

    pub(super) fn flags_case(&self, name: &str) -> Option<&'a FlagsInfo> {
        self.lookup_ref(name, Some(self.local_flags_cases), self.all_flags_cases)
    }

    pub(super) fn resource_type(&self, name: &str) -> Option<&'a ResourceInfo> {
        self.lookup_ref(name, None, self.all_resource_types)
    }

    pub(super) fn generic_newtype(&self, name: &str) -> Option<&'a GenericNewtypeInfo> {
        self.lookup_ref(
            name,
            Some(self.local_generic_newtypes),
            self.all_generic_newtypes,
        )
    }

    pub(super) fn newtype(&self, name: &str) -> Option<TypeId> {
        self.lookup_ref(name, Some(self.local_newtypes), self.all_newtypes)
            .copied()
    }
}

/// Info about an Index trait implementation
pub(super) struct IndexTraitInfo {
    /// The Output associated type
    pub(super) output_type: TypeId,
    /// Self kind for the index method (&self)
    pub(super) self_kind: ast::SelfKind,
    /// The trait name (e.g., "Index<i32>")
    pub(super) trait_name: String,
    /// Module where the impl block is defined
    pub(super) impl_module_source: ModuleSource,
}

/// Info about an `IndexAssign` trait implementation
pub(super) struct IndexAssignTraitInfo {
    /// The Input associated type
    pub(super) input_type: TypeId,
    /// Self kind for the `index_assign` method (&mut self)
    pub(super) self_kind: ast::SelfKind,
    /// The trait name (e.g., "`IndexAssign`<i32>")
    pub(super) trait_name: String,
    /// Module where the impl block is defined
    pub(super) impl_module_source: ModuleSource,
}

/// Info about an `IndexMut` trait implementation
pub(super) struct IndexMutTraitInfo {
    /// The Output associated type
    pub(super) output_type: TypeId,
    /// Self kind for the `index_mut` method (&mut self)
    pub(super) self_kind: ast::SelfKind,
    /// The trait name (e.g., "`IndexMut`")
    pub(super) trait_name: String,
    /// Module where the impl block is defined
    pub(super) impl_module_source: ModuleSource,
}

/// Info about an `IndexValue` trait implementation
pub(super) struct IndexValueTraitInfo {
    /// The Output associated type
    pub(super) output_type: TypeId,
    /// Self kind for the `index_value` method (&self)
    pub(super) self_kind: ast::SelfKind,
    /// The trait name (e.g., "`IndexValue`<i32>")
    pub(super) trait_name: String,
    /// Module where the impl block is defined
    pub(super) impl_module_source: ModuleSource,
}

/// Info about a comparison trait implementation (`Eq` or `Ord`)
/// Info about an operator trait implementation
pub(super) struct ArithmeticTraitInfo {
    /// The Output associated type
    pub(super) output_type: TypeId,
    /// Self kind for the method (&self)
    pub(super) self_kind: ast::SelfKind,
    /// The trait name (e.g., "Add", "Sub")
    pub(super) trait_name: String,
    /// The resolved type of the rhs parameter (first non-self parameter)
    pub(super) rhs_type: Option<TypeId>,
}

/// Complete, Self-substituted description of a trait method lookup.
///
/// Produced by [`Resolver::resolve_trait_method_for_op`][rtq] and consumed
/// by [`Resolver::build_trait_op_method_call_on_resolved`][bop]. Having a
/// single, always-populated data type for trait-method dispatch eliminates
/// the `param_types: vec![]` anti-pattern that previously caused codegen
/// ICEs when operator dispatch built `TirExprKind::MethodCall` without any
/// argument-type check.
///
/// [rtq]: crate::resolver::Resolver::resolve_trait_method_for_op
/// [bop]: crate::resolver::Resolver::build_trait_op_method_call_on_resolved
pub(super) struct ResolvedTraitMethod {
    /// Trait name (e.g., "Eq", "Ord", "Add", "Shl", "Neg", "`BitNot`").
    pub(super) trait_name: String,
    /// Method name (e.g., "eq", "cmp", "add", "shl", "neg", "bitnot").
    pub(super) method_name: String,
    /// Struct name used for method mangling. For newtypes this may be the
    /// ultimate base-type name when dispatch falls back to the base impl.
    pub(super) impl_name: String,
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

/// Info about a `KeyValueLiteralBuilder` trait implementation
pub(super) struct KeyValueLiteralTraitInfo {
    /// The Value associated type (element type for literal values)
    pub(super) value_type: TypeId,
    /// The Builder type (the type that accumulates key-value pairs)
    pub(super) builder_type: TypeId,
    /// Self kind for the `insert_literal` method (&mut self)
    pub(super) self_kind: ast::SelfKind,
    /// The trait name used for method mangling (e.g., "`KeyValueLiteralBuilder`")
    pub(super) trait_name: String,
}

/// Info about a `SequenceLiteralBuilder` trait implementation
pub(super) struct SequenceLiteralTraitInfo {
    /// The Element associated type (element type for literal values)
    pub(super) element_type: TypeId,
    /// The Builder type (the type that accumulates elements)
    pub(super) builder_type: TypeId,
    /// The Output type (the final type after `build()`)
    pub(super) output_type: TypeId,
    /// Self kind for the `push_literal` method (&mut self)
    pub(super) self_kind: ast::SelfKind,
    /// The trait name used for method mangling (e.g., "`SequenceLiteralBuilder`")
    pub(super) trait_name: String,
    /// The module where the `SequenceLiteralBuilder` impl is defined.
    pub(super) impl_module_source: ModuleSource,
}
