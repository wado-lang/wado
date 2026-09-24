//! The attribute schema: every `#[…]` and `#![…]` the compiler recognizes,
//! where it may be written, and what arguments it takes.

use crate::ast::{
    AstVisitor, AttrArg, Attribute, Expr, Function, GenericParam, InnerAttribute, Item, Module,
    Stmt, WorldExport, walk_expr, walk_function, walk_generic_params, walk_item, walk_stmt,
};
use crate::token::Span;

pub const ALLOC: &str = "allocator";
pub const ALLOW: &str = "allow";
pub const AMBIENT: &str = "ambient";
pub const BENIGN: &str = "benign";
pub const CANONICAL: &str = "canonical";
pub const CM: &str = "cm";
pub const CM_HOST_IMPORTS: &str = "cm_host_imports";
pub const CM_PARAMS: &str = "cm_params";
pub const COMPILER_ITEM: &str = "compiler_item";
pub const EXPECT_TRAP: &str = "expect_trap";
pub const EXPORT: &str = "export";
pub const EXPORT_NAME: &str = "export_name";
pub const GENERATED: &str = "generated";
pub const IMMEDIATE: &str = "immediate";
pub const INLINE: &str = "inline";
pub const LINEAR_MEMORY: &str = "linear_memory";
pub const NO_PRELUDE: &str = "no_prelude";
pub const PARAM: &str = "param";
pub const RESULT: &str = "result";
pub const RETAIN: &str = "retain";
pub const SECRET: &str = "secret";
pub const STDLIB: &str = "stdlib";
pub const SYNOPSIS: &str = "synopsis";
pub const TIMEOUT_MS: &str = "timeout_ms";
pub const TODO: &str = "TODO";
pub const TRAP: &str = "trap";
/// See [WEP: Declared Absence](../../docs/wep-2026-09-13-declared-absence.md).
pub const UNAVAILABLE: &str = "unavailable";
pub const WASM_MODULE: &str = "wasm_module";
pub const WIRE: &str = "wire";

/// A place an attribute may be written. `Module` is the inner-attribute
/// position (`#![…]`); every other target takes an outer `#[…]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrTarget {
    Module,
    Function,
    Param,
    GenericParam,
    Global,
    Let,
    Test,
    Struct,
    StructField,
    Enum,
    EnumCase,
    Variant,
    VariantCase,
    Flags,
    FlagsVariant,
    Newtype,
    TupleType,
    BuiltinType,
    Trait,
    Interface,
    Resource,
    World,
    Impl,
    AssociatedConst,
    AssociatedType,
    Use,
}

impl AttrTarget {
    /// How a diagnostic names this place, article and all.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Module => "a module",
            Self::Function => "a function",
            Self::Param => "a parameter",
            Self::GenericParam => "a type parameter",
            Self::Global => "a global",
            Self::Let => "a let statement",
            Self::Test => "a test block",
            Self::Struct => "a struct",
            Self::StructField => "a struct field",
            Self::Enum => "an enum",
            Self::EnumCase => "an enum case",
            Self::Variant => "a variant",
            Self::VariantCase => "a variant case",
            Self::Flags => "a flags declaration",
            Self::FlagsVariant => "a flags member",
            Self::Newtype => "a newtype",
            Self::TupleType => "a tuple type declaration",
            Self::BuiltinType => "a builtin type declaration",
            Self::Trait => "a trait",
            Self::Interface => "an interface",
            Self::Resource => "a resource",
            Self::World => "a world",
            Self::Impl => "an impl block",
            Self::AssociatedConst => "an associated const",
            Self::AssociatedType => "an associated type",
            Self::Use => "a use declaration",
        }
    }

    /// Whether this target takes `#![…]` rather than `#[…]`.
    #[must_use]
    pub fn is_inner(self) -> bool {
        matches!(self, Self::Module)
    }
}

/// The arguments an attribute takes. [`AttrArgs::Read`] is the shape no central
/// rule captures, checked where the attribute is read — for `#[cm]` and
/// `#[canonical]` that reader is the parser, which turns them into a
/// `CmBoundary` on the attribute itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrArgs {
    /// Nothing, as `#[expect_trap]`.
    None,
    /// One string literal, as `#[export_name("run")]`.
    OneString,
    /// One or more string literals, as `#[canonical("wasi", "stream-new")]`.
    Strings,
    /// String literals, or nothing at all: `#[cm_params()]` is what a
    /// zero-parameter CM function names.
    OptionalStrings,
    /// One numeric literal, as `#[timeout_ms(5000)]`.
    OneNumber,
    /// One or more bare identifiers, as `#[allow(dead_code)]`. A lint name is
    /// read as an identifier alone, so a quoted one waives nothing.
    Idents,
    /// One or more words, quoted or bare, as `#[benign(Stdout)]`.
    Words,
    /// Words, or nothing at all, as `#[inline]` / `#[inline("never")]`.
    OptionalWords,
    /// A shape of its own, described here and checked where it is read.
    Read(&'static str),
}

impl AttrArgs {
    /// How a diagnostic spells what this attribute takes.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::None => "no arguments",
            Self::OneString => "one string",
            Self::Strings => "one or more strings",
            Self::OptionalStrings => "strings, or nothing",
            Self::OneNumber => "one number",
            Self::Idents => "one or more bare names",
            Self::Words => "one or more names, quoted or bare",
            Self::OptionalWords => "names, quoted or bare, or nothing",
            Self::Read(shape) => shape,
        }
    }

    /// Whether `args` is what this shape takes. A [`Self::Read`] shape admits
    /// every argument list here, and is checked where it is read.
    fn admits(self, args: &[AttrArg]) -> bool {
        match self {
            Self::Read(_) => true,
            Self::None => args.is_empty(),
            Self::OneString => matches!(args, [AttrArg::Str(_)]),
            Self::Strings => {
                !args.is_empty() && args.iter().all(|arg| matches!(arg, AttrArg::Str(_)))
            }
            Self::OptionalStrings => args.iter().all(|arg| matches!(arg, AttrArg::Str(_))),
            Self::OneNumber => matches!(args, [AttrArg::Number(_)]),
            Self::Idents => {
                !args.is_empty() && args.iter().all(|arg| matches!(arg, AttrArg::Ident(_)))
            }
            Self::Words => !args.is_empty() && Self::OptionalWords.admits(args),
            Self::OptionalWords => args
                .iter()
                .all(|arg| matches!(arg, AttrArg::Ident(_) | AttrArg::Str(_))),
        }
    }
}

/// One attribute's schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttributeSchema {
    pub name: &'static str,
    /// Where the attribute may be written, or [`EVERY_TARGET`] for anywhere.
    pub targets: &'static [AttrTarget],
    pub args: AttrArgs,
    /// What the attribute declares, as a diagnostic spells it.
    pub summary: &'static str,
    /// Set for an attribute that describes a declaration with no body.
    pub bodyless: Option<Bodyless>,
}

/// Why a bodyless attribute is refused where it is written: on a function
/// with a body, and on a `trait` or `interface` method requirement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bodyless {
    pub on_body: &'static str,
    pub on_requirement: &'static str,
}

/// Every place an attribute can be written, which `#[allow(…)]` reaches. The
/// list is empty so a new [`AttrTarget`] belongs to it without being added.
const EVERY_TARGET: &[AttrTarget] = &[];

/// Every declaration that crosses the Component Model boundary, which is what
/// `#[cm(…)]` names on the CM side.
const CM_TARGET: &[AttrTarget] = &[
    AttrTarget::Function,
    AttrTarget::Struct,
    AttrTarget::StructField,
    AttrTarget::Enum,
    AttrTarget::EnumCase,
    AttrTarget::Variant,
    AttrTarget::VariantCase,
    AttrTarget::Flags,
    AttrTarget::FlagsVariant,
    AttrTarget::Newtype,
    AttrTarget::Interface,
    AttrTarget::Resource,
    AttrTarget::World,
];

/// Every declaration a `#[compiler_item("…")]` can bind. The registry checks
/// each item against the kind it expects; this is the outer bound.
const COMPILER_ITEM_TARGET: &[AttrTarget] = &[
    AttrTarget::Function,
    AttrTarget::Struct,
    AttrTarget::Enum,
    AttrTarget::EnumCase,
    AttrTarget::Variant,
    AttrTarget::VariantCase,
    AttrTarget::Newtype,
    AttrTarget::TupleType,
    AttrTarget::BuiltinType,
    AttrTarget::Trait,
    AttrTarget::Interface,
    AttrTarget::Resource,
];

/// Every declaration whose serialized form `#[wire(…)]` customizes.
const WIRE_TARGET: &[AttrTarget] = &[
    AttrTarget::Struct,
    AttrTarget::StructField,
    AttrTarget::Enum,
    AttrTarget::EnumCase,
    AttrTarget::Variant,
    AttrTarget::VariantCase,
    AttrTarget::Flags,
    AttrTarget::FlagsVariant,
    AttrTarget::Newtype,
];

const FUNCTION_TARGET: &[AttrTarget] = &[AttrTarget::Function];
const TEST_TARGET: &[AttrTarget] = &[AttrTarget::Test];
const MODULE_TARGET: &[AttrTarget] = &[AttrTarget::Module];

/// The valid attributes, in the order a reference lists them. The single source
/// of truth: a name absent here is rejected where it is written.
pub const ATTRIBUTES: &[AttributeSchema] = &[
    AttributeSchema {
        name: ALLOC,
        targets: FUNCTION_TARGET,
        args: AttrArgs::OneString,
        summary: "the allocator this entry point installs",
        bodyless: None,
    },
    AttributeSchema {
        name: ALLOW,
        targets: EVERY_TARGET,
        args: AttrArgs::Idents,
        summary: "waive a lint here",
        bodyless: None,
    },
    AttributeSchema {
        name: AMBIENT,
        targets: FUNCTION_TARGET,
        args: AttrArgs::None,
        summary: "perform this declaration's effects without declaring them",
        bodyless: None,
    },
    AttributeSchema {
        name: BENIGN,
        targets: FUNCTION_TARGET,
        args: AttrArgs::Words,
        summary: "effects a caller need not declare onward",
        bodyless: None,
    },
    AttributeSchema {
        name: CANONICAL,
        targets: FUNCTION_TARGET,
        args: AttrArgs::Read("a namespace and a name, as two strings"),
        summary: "the Component Model canonical built-in this lowers to",
        bodyless: None,
    },
    AttributeSchema {
        name: CM,
        targets: CM_TARGET,
        args: AttrArgs::Read("the CM-side identifier, with an optional `linearity = \"…\"`"),
        summary: "this declaration's identity at the Component Model boundary",
        bodyless: None,
    },
    AttributeSchema {
        name: CM_HOST_IMPORTS,
        targets: MODULE_TARGET,
        args: AttrArgs::Strings,
        summary: "the host capabilities an imported component itself imports",
        bodyless: None,
    },
    AttributeSchema {
        name: CM_PARAMS,
        targets: FUNCTION_TARGET,
        args: AttrArgs::OptionalStrings,
        summary: "the CM-side parameter names",
        bodyless: None,
    },
    AttributeSchema {
        name: COMPILER_ITEM,
        targets: COMPILER_ITEM_TARGET,
        args: AttrArgs::OneString,
        summary: "bind this stdlib declaration to the compiler item of that name",
        bodyless: None,
    },
    AttributeSchema {
        name: EXPECT_TRAP,
        targets: TEST_TARGET,
        args: AttrArgs::None,
        summary: "the test passes when its body traps",
        bodyless: None,
    },
    AttributeSchema {
        name: EXPORT,
        targets: FUNCTION_TARGET,
        args: AttrArgs::None,
        summary: "a raw Wasm export, and so an export-boundary root",
        bodyless: None,
    },
    AttributeSchema {
        name: EXPORT_NAME,
        targets: FUNCTION_TARGET,
        args: AttrArgs::OneString,
        summary: "the name this function is exported under",
        bodyless: None,
    },
    AttributeSchema {
        name: GENERATED,
        targets: MODULE_TARGET,
        args: AttrArgs::Read("`by = \"…\"` and `sources = [\"…\"]` metadata"),
        summary: "the file is machine-generated",
        bodyless: None,
    },
    AttributeSchema {
        name: IMMEDIATE,
        targets: FUNCTION_TARGET,
        args: AttrArgs::Read("one parameter name, unquoted"),
        summary: "the parameter lowers to a Wasm immediate",
        bodyless: Some(Bodyless {
            on_body: "it describes how codegen lowers the call, and a body is called \
                      rather than lowered",
            on_requirement: "it says how codegen lowers one call, and a requirement is \
                             dispatched to an impl that is called rather than lowered",
        }),
    },
    AttributeSchema {
        name: INLINE,
        targets: FUNCTION_TARGET,
        args: AttrArgs::OptionalWords,
        summary: "how the inliner should treat this function",
        bodyless: None,
    },
    AttributeSchema {
        name: LINEAR_MEMORY,
        targets: FUNCTION_TARGET,
        args: AttrArgs::Read("`read` or `write`"),
        summary: "how a call to this declaration touches linear memory",
        bodyless: Some(Bodyless {
            on_body: "a body states what it touches",
            on_requirement: "the impl it dispatches to has the body that says what it \
                             touches, so stating it here binds every call site to a \
                             promise no implementation makes",
        }),
    },
    AttributeSchema {
        name: NO_PRELUDE,
        targets: MODULE_TARGET,
        args: AttrArgs::None,
        summary: "the file imports no prelude",
        bodyless: None,
    },
    AttributeSchema {
        name: PARAM,
        targets: &[AttrTarget::Global],
        args: AttrArgs::Read("`name = \"…\"` and `from_env = \"…\"`"),
        summary: "the global is a build input",
        bodyless: None,
    },
    AttributeSchema {
        name: RESULT,
        targets: FUNCTION_TARGET,
        args: AttrArgs::Read("`owned`, or `part_of = param`"),
        summary: "what the returned storage belongs to",
        bodyless: Some(Bodyless {
            on_body: "a body states what it returns",
            on_requirement: "the impl it dispatches to has the body that says what its \
                             result is made of, so stating it here binds every call site \
                             to a promise no implementation makes",
        }),
    },
    AttributeSchema {
        name: RETAIN,
        targets: FUNCTION_TARGET,
        args: AttrArgs::Read("a parameter name, with an optional `into = param`"),
        summary: "what this declaration retains a reference to",
        bodyless: Some(Bodyless {
            on_body: "a body states what it retains",
            on_requirement: "the impl it dispatches to has the body that says what it \
                             keeps, so stating it here binds every call site to a \
                             promise no implementation makes",
        }),
    },
    AttributeSchema {
        name: SECRET,
        targets: &[AttrTarget::StructField],
        args: AttrArgs::None,
        summary: "the field's value stays out of debug output",
        bodyless: None,
    },
    AttributeSchema {
        name: STDLIB,
        targets: MODULE_TARGET,
        args: AttrArgs::OneString,
        summary: "the bundled stdlib path this file is",
        bodyless: None,
    },
    AttributeSchema {
        name: SYNOPSIS,
        targets: TEST_TARGET,
        args: AttrArgs::None,
        summary: "`wado doc` renders this test as the module's synopsis",
        bodyless: None,
    },
    AttributeSchema {
        name: TIMEOUT_MS,
        targets: TEST_TARGET,
        args: AttrArgs::OneNumber,
        summary: "how long the test may run",
        bodyless: None,
    },
    AttributeSchema {
        name: TODO,
        targets: &[AttrTarget::Module, AttrTarget::Test],
        args: AttrArgs::None,
        summary: "the tests are expected to fail until the work lands",
        bodyless: None,
    },
    AttributeSchema {
        name: TRAP,
        targets: FUNCTION_TARGET,
        args: AttrArgs::Read(
            "`never`, or `negative = p` / `outside = a` (with `at = i`, `len = n`) / `unset = a`, \
             with an optional `result_len = p`",
        ),
        summary: "when a call to this declaration traps",
        bodyless: Some(Bodyless {
            on_body: "a body states when it traps",
            on_requirement: "the impl it dispatches to has the body that says when it \
                             traps, so stating it here binds every call site to a \
                             promise no implementation makes",
        }),
    },
    AttributeSchema {
        name: UNAVAILABLE,
        targets: FUNCTION_TARGET,
        args: AttrArgs::OneString,
        summary: "the name is reserved, and why a call cannot have it",
        bodyless: None,
    },
    AttributeSchema {
        name: WASM_MODULE,
        targets: MODULE_TARGET,
        args: AttrArgs::OneString,
        summary: "the core wasm module name this file imports memory from",
        bodyless: None,
    },
    AttributeSchema {
        name: WIRE,
        targets: WIRE_TARGET,
        args: AttrArgs::Read(
            "`name = \"…\"`, `name_policy = \"…\"`, `number = N`, `positional`, or `default`",
        ),
        summary: "how serialization spells this declaration",
        bodyless: None,
    },
];

/// The schema for `name`, or `None` where no attribute is written that way.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static AttributeSchema> {
    ATTRIBUTES.iter().find(|schema| schema.name == name)
}

/// The attributes among `attrs` that describe a declaration with no body.
pub fn bodyless(attrs: &[Attribute]) -> impl Iterator<Item = (&Attribute, &'static str, Bodyless)> {
    attrs.iter().filter_map(|attr| {
        let schema = lookup(&attr.name)?;
        Some((attr, schema.name, schema.bodyless?))
    })
}

/// Why an attribute as written is not the one the schema describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeFault {
    /// No attribute is written that way.
    Unknown,
    /// Written somewhere the attribute does not belong.
    Misplaced {
        target: AttrTarget,
        schema: &'static AttributeSchema,
    },
    /// Written with `#[…]` where it takes `#![…]`, or the other way round.
    Position {
        target: AttrTarget,
        schema: &'static AttributeSchema,
    },
    /// The wrong number of arguments, or the wrong kind.
    Arguments { schema: &'static AttributeSchema },
}

impl AttributeFault {
    /// The sentence this fault reports.
    #[must_use]
    pub fn message(&self, name: &str) -> String {
        match self {
            Self::Unknown => format!("unknown attribute: `{name}`"),
            Self::Misplaced { target, schema } => format!(
                "`{name}` does not belong on {}; it belongs on {}",
                target.describe(),
                list_targets(schema.targets)
            ),
            Self::Position { target, schema } => {
                if target.is_inner() {
                    format!(
                        "`#![{name}]` does not belong to the module; write `#[{name}]` on {}",
                        list_targets(schema.targets)
                    )
                } else {
                    format!(
                        "`#[{name}]` belongs to the module — {}; write `#![{name}]` at the top of the file",
                        schema.summary
                    )
                }
            }
            Self::Arguments { schema } => {
                format!("`{name}` takes {}", schema.args.describe())
            }
        }
    }
}

/// The targets a diagnostic lists, as prose.
fn list_targets(targets: &[AttrTarget]) -> String {
    if targets.is_empty() {
        return "any declaration".to_string();
    }
    let mut out = String::new();
    for (index, target) in targets.iter().enumerate() {
        if index > 0 {
            out.push_str(if index + 1 == targets.len() {
                " or "
            } else {
                ", "
            });
        }
        out.push_str(target.describe());
    }
    out
}

/// What is wrong with an attribute named `name` carrying `args` at `target`,
/// where a central rule decides. `None` means the schema admits it.
#[must_use]
pub fn check(name: &str, args: &[AttrArg], target: AttrTarget) -> Option<AttributeFault> {
    let Some(schema) = lookup(name) else {
        return Some(AttributeFault::Unknown);
    };
    if !schema.targets.is_empty() && !schema.targets.contains(&target) {
        // An attribute that belongs only in the other position is written in
        // the wrong one, which says what to write instead of where.
        let wrong_position = schema
            .targets
            .iter()
            .all(|admitted| admitted.is_inner() != target.is_inner());
        let fault = if wrong_position {
            AttributeFault::Position { target, schema }
        } else {
            AttributeFault::Misplaced { target, schema }
        };
        return Some(fault);
    }
    if schema.args.admits(args) {
        return None;
    }
    Some(AttributeFault::Arguments { schema })
}

/// One attribute as written, whatever position it sits in.
#[derive(Debug, Clone, Copy)]
pub struct WrittenAttribute<'a> {
    pub name: &'a str,
    pub args: &'a [AttrArg],
    pub target: AttrTarget,
    pub span: Span,
}

impl<'a> WrittenAttribute<'a> {
    fn outer(attr: &'a Attribute, target: AttrTarget) -> Self {
        Self {
            name: &attr.name,
            args: &attr.args,
            target,
            span: attr.span,
        }
    }

    fn inner(attr: &'a InnerAttribute) -> Self {
        Self {
            name: &attr.name,
            args: &attr.args,
            target: AttrTarget::Module,
            span: attr.span,
        }
    }
}

/// Call `f` for every attribute written in `module`, with the place it sits.
pub fn for_each_attribute(module: &Module, mut f: impl FnMut(WrittenAttribute<'_>)) {
    for attr in module.inner_attributes() {
        f(WrittenAttribute::inner(attr));
    }
    let mut walk = AttributeWalk { report: &mut f };
    for item in &module.items {
        walk.visit_item(item);
    }
}

struct AttributeWalk<'a, F: FnMut(WrittenAttribute<'_>)> {
    report: &'a mut F,
}

impl<F: FnMut(WrittenAttribute<'_>)> AttributeWalk<'_, F> {
    fn report(&mut self, attrs: &[Attribute], target: AttrTarget) {
        for attr in attrs {
            (self.report)(WrittenAttribute::outer(attr, target));
        }
    }
}

impl<F: FnMut(WrittenAttribute<'_>)> AstVisitor for AttributeWalk<'_, F> {
    fn visit_item(&mut self, item: &Item) {
        match item {
            Item::Struct(decl) => {
                self.report(&decl.attrs, AttrTarget::Struct);
                for field in &decl.fields {
                    self.report(&field.attrs, AttrTarget::StructField);
                }
            }
            Item::Enum(decl) => {
                self.report(&decl.attrs, AttrTarget::Enum);
                for case in &decl.cases {
                    self.report(&case.attrs, AttrTarget::EnumCase);
                }
            }
            Item::Variant(decl) => {
                self.report(&decl.attrs, AttrTarget::Variant);
                for case in &decl.cases {
                    self.report(&case.attrs, AttrTarget::VariantCase);
                }
            }
            Item::Flags(decl) => {
                if let Some(attrs) = &decl.attributes {
                    self.report(attrs, AttrTarget::Flags);
                }
                for member in &decl.flags {
                    self.report(&member.attrs, AttrTarget::FlagsVariant);
                }
            }
            Item::Newtype(decl) => self.report(&decl.attrs, AttrTarget::Newtype),
            Item::TupleTypeDecl(decl) => self.report(&decl.attrs, AttrTarget::TupleType),
            Item::BuiltinTypeDecl(decl) => self.report(&decl.attrs, AttrTarget::BuiltinType),
            Item::Trait(decl) => self.report(&decl.attrs, AttrTarget::Trait),
            Item::Interface(decl) => self.report(&decl.attrs, AttrTarget::Interface),
            Item::Resource(decl) => self.report(&decl.attrs, AttrTarget::Resource),
            Item::World(decl) => {
                self.report(&decl.attrs, AttrTarget::World);
                for export in &decl.exports {
                    if let WorldExport::Function(func) = export {
                        for param in &func.params {
                            self.report(&param.attrs, AttrTarget::Param);
                        }
                    }
                }
            }
            Item::Test(decl) => self.report(&decl.attributes, AttrTarget::Test),
            Item::Global(decl) => self.report(&decl.attributes, AttrTarget::Global),
            Item::Impl(decl) => {
                self.report(&decl.attrs, AttrTarget::Impl);
                for binding in &decl.associated_types {
                    self.report(&binding.attrs, AttrTarget::AssociatedType);
                }
                for constant in &decl.constants {
                    self.report(&constant.attrs, AttrTarget::AssociatedConst);
                }
            }
            Item::Use(decl) => self.report(&decl.attrs, AttrTarget::Use),
            Item::Function(_) | Item::Error(_) => {}
        }
        walk_item(self, item);
    }

    fn visit_function(&mut self, func: &Function) {
        self.report(&func.attrs, AttrTarget::Function);
        for param in &func.params {
            self.report(&param.attrs, AttrTarget::Param);
        }
        walk_function(self, func);
    }

    fn visit_generic_params(&mut self, params: &[GenericParam]) {
        for param in params {
            self.report(&param.attrs, AttrTarget::GenericParam);
        }
        walk_generic_params(self, params);
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        if let Stmt::Let(let_stmt) = stmt {
            self.report(&let_stmt.attrs, AttrTarget::Let);
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        if let Expr::Closure(closure) = expr {
            for param in &closure.params {
                self.report(&param.attrs, AttrTarget::Param);
            }
        }
        walk_expr(self, expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_sorted_case_insensitively() {
        let names: Vec<String> = ATTRIBUTES
            .iter()
            .map(|schema| schema.name.to_lowercase())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(names, sorted, "ATTRIBUTES must be unique and sorted");
    }

    /// An empty target list is [`EVERY_TARGET`], not "nowhere", so only
    /// `#[allow]` may carry one.
    #[test]
    fn only_allow_reaches_every_target() {
        for schema in ATTRIBUTES {
            assert_eq!(
                schema.targets.is_empty(),
                schema.name == ALLOW,
                "`{}` names the wrong target list",
                schema.name
            );
        }
        for target in [AttrTarget::Impl, AttrTarget::Use, AttrTarget::Module] {
            assert!(check(ALLOW, &[AttrArg::Ident("dead_code".to_string())], target).is_none());
        }
    }

    /// Only a function can have a body, or be a requirement.
    #[test]
    fn only_function_attributes_are_bodyless() {
        for schema in ATTRIBUTES {
            if schema.bodyless.is_some() {
                assert_eq!(schema.targets, FUNCTION_TARGET, "`{}`", schema.name);
            }
        }
    }

    /// What a fixture cannot state: the `Read` shapes reach their reader
    /// unjudged, however they are written. The faults themselves are stated by
    /// the `attr_*.wado` fixtures, in the source they reject.
    #[test]
    fn a_read_shape_is_left_to_its_reader() {
        assert!(check(PARAM, &[], AttrTarget::Global).is_none());
        assert!(
            check(
                PARAM,
                &[AttrArg::KeyValue(
                    "from_env".to_string(),
                    "PORT".to_string()
                )],
                AttrTarget::Global
            )
            .is_none()
        );
    }
}
