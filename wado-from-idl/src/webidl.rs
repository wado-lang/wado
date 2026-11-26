//! WebIDL-to-IR transformation, over the webidl2 AST `tools/webidl/snapshot.mjs`
//! writes: one unrestricted resource per interface. See `docs/wep-2026-04-01-web.md`.

use std::cell::RefCell;

use anyhow::{Result, anyhow, bail};
use indexmap::{IndexMap, IndexSet};
use serde::Deserialize;
use wado_compiler::ast::HandleClasses;

use crate::WadoCodeGenerator;
use crate::glue;
use crate::ir::{
    DictionaryField, UnionCase, WadoDictionary, WadoEnum, WadoEnumVariant, WadoField, WadoFunction,
    WadoInterface, WadoModule, WadoParam, WadoResource, WadoStruct, WadoType, WadoTypeDef,
    WadoUnion, WadoVariant, WadoVariantCase,
};
use crate::naming::{to_kebab_case, to_snake_case, to_upper_camel_case, to_wado_identifier};

/// The interface of the functions handing out the first handle.
const GLOBAL_INTERFACE: &str = "global";

/// The resource a rejected promise's reason is, which is no `WebIDL` interface:
/// a reason is any value.
const REJECTION: &str = "Rejection";

/// The CM interface the module's enums are declared in.
const TYPES_INTERFACE: &str = "types";

/// The file `snapshot.mjs` writes: the slice's definitions, in webidl2's shape.
#[derive(Deserialize)]
pub struct Snapshot {
    /// The `@webref/idl` version the slice was taken from.
    pub webref: String,
    /// The CM package every interface is imported from, e.g. `wado-lang:web`.
    pub package: String,
    /// The module the slice generates, which names its files and `[Global]` effect.
    pub module: String,
    /// The interfaces to generate, in output order.
    pub slice: Vec<String>,
    /// Every `interface` and `partial interface` of a slice member.
    pub interfaces: Vec<Interface>,
    /// Every `interface mixin` (and partial) a slice member includes.
    pub mixins: Vec<Interface>,
    /// The `X includes M` statements whose target is in the slice.
    pub includes: Vec<Includes>,
    pub typedefs: Vec<Typedef>,
    pub callbacks: Vec<Callback>,
    /// Every `dictionary` (and partial) a member of the slice names.
    pub dictionaries: Vec<Dictionary>,
    /// Every `enum` a member of the slice names.
    pub enums: Vec<Enum>,
    /// Every `namespace` (and partial) the snapshot was asked for.
    pub namespaces: Vec<Interface>,
}

#[derive(Deserialize)]
pub struct Dictionary {
    pub name: String,
    pub partial: bool,
    pub inheritance: Option<String>,
    pub members: Vec<Field>,
}

#[derive(Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(rename = "idlType")]
    pub idl_type: IdlType,
    pub required: bool,
}

#[derive(Deserialize)]
pub struct Enum {
    pub name: String,
    pub values: Vec<EnumValue>,
}

#[derive(Deserialize)]
pub struct EnumValue {
    pub value: String,
}

impl Enum {
    /// The Wado case each value names, in order, or why the enum has none.
    fn cases(&self) -> std::result::Result<Vec<String>, Reason> {
        assert!(
            self.values.len() <= MAX_ENUM_CASES,
            "`{}` has more cases than one byte tells apart",
            self.name
        );
        let mut cases: IndexMap<String, &str> = IndexMap::new();
        for EnumValue { value } in &self.values {
            let case = to_upper_camel_case(value);
            if !case.starts_with(|first: char| first.is_ascii_uppercase()) {
                return Err(Reason::new(
                    Cause::Enum,
                    format!("the value `{value}` names no case"),
                ));
            }
            if let Some(first) = cases.insert(case.clone(), value) {
                return Err(Reason::new(
                    Cause::Enum,
                    format!("the values `{first}` and `{value}` both name the case `{case}`"),
                ));
            }
        }
        Ok(cases.into_keys().collect())
    }
}

/// What the glue reads and writes an enum in memory as: one byte.
const MAX_ENUM_CASES: usize = 1 << 8;

#[derive(Deserialize)]
pub struct Interface {
    pub name: String,
    pub partial: bool,
    pub inheritance: Option<String>,
    #[serde(rename = "extAttrs")]
    pub ext_attrs: Vec<ExtAttr>,
    pub members: Vec<Member>,
}

#[derive(Deserialize)]
pub struct ExtAttr {
    pub name: String,
}

#[derive(Deserialize)]
pub struct Includes {
    pub target: String,
    pub includes: String,
}

#[derive(Deserialize)]
pub struct Typedef {
    pub name: String,
    #[serde(rename = "idlType")]
    pub idl_type: IdlType,
}

/// A function the page calls back: a `callback`, or a `callback interface`,
/// whose one operation a function stands in for.
#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum Callback {
    #[serde(rename = "callback")]
    Function {
        name: String,
        #[serde(rename = "idlType")]
        idl_type: IdlType,
        arguments: Vec<Argument>,
    },
    #[serde(rename = "callback interface")]
    Interface { name: String, members: Vec<Member> },
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum Member {
    #[serde(rename = "attribute")]
    Attribute {
        name: String,
        #[serde(rename = "idlType")]
        idl_type: IdlType,
        readonly: bool,
        special: String,
    },
    #[serde(rename = "operation")]
    Operation {
        name: String,
        /// Absent on a bare `stringifier;`.
        #[serde(rename = "idlType")]
        idl_type: Option<IdlType>,
        arguments: Vec<Argument>,
        special: String,
    },
    #[serde(rename = "constructor")]
    Constructor {
        arguments: Vec<Argument>,
        #[serde(rename = "extAttrs")]
        ext_attrs: Vec<ExtAttr>,
    },
    #[serde(rename = "const")]
    Const { name: String },
    #[serde(rename = "iterable")]
    Iterable,
    #[serde(rename = "maplike")]
    Maplike,
    #[serde(rename = "setlike")]
    Setlike,
    #[serde(rename = "async_iterable")]
    AsyncIterable,
}

impl Member {
    /// What a report calls the member: its `WebIDL` name, or what stands in for one.
    fn label(&self) -> String {
        match self {
            Self::Attribute { name, .. } | Self::Const { name } => name.clone(),
            Self::Operation { name, special, .. } if name.is_empty() => format!("({special})"),
            Self::Operation { name, .. } => name.clone(),
            Self::Constructor { .. } => "new".to_string(),
            Self::Iterable => "(iterable)".to_string(),
            Self::Maplike => "(maplike)".to_string(),
            Self::Setlike => "(setlike)".to_string(),
            Self::AsyncIterable => "(async_iterable)".to_string(),
        }
    }
}

#[derive(Deserialize)]
pub struct Argument {
    pub name: String,
    #[serde(rename = "idlType")]
    pub idl_type: IdlType,
    pub optional: bool,
    pub variadic: bool,
    pub default: Option<DefaultValue>,
}

/// An optional argument's default, as the snapshot records it.
#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum DefaultValue {
    #[serde(rename = "boolean")]
    Boolean { value: bool },
    #[serde(rename = "number")]
    Number { value: String },
    #[serde(rename = "string")]
    String { value: String },
    /// `null`, `{}`, `[]`, `Infinity`, `NaN`: the browser's own default.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub struct IdlType {
    pub generic: String,
    pub nullable: bool,
    #[serde(rename = "idlType")]
    pub inner: IdlTypeInner,
}

/// A name, or the constituents of a union or of a generic's arguments.
#[derive(Deserialize)]
#[serde(untagged)]
pub enum IdlTypeInner {
    Name(String),
    Types(Vec<IdlType>),
}

/// Which way a value crosses the boundary. It decides whether collapsing a
/// union to its one expressible constituent is lossless: on the way in the
/// dropped constituents are types the slice cannot build, on the way out they
/// are values the host may hand back.
#[derive(Clone, Copy, PartialEq)]
enum Flow {
    In,
    Out,
}

/// The generated module, what the glue does for each of its functions, and
/// every member the slice could not express.
#[derive(Debug)]
pub struct WebIdlOutput {
    pub module: WadoModule,
    /// Each function's JavaScript half, keyed by its `#[cm]` path.
    pub js: IndexMap<String, JsFunction>,
    /// Each resource's `WebIDL` interface name, keyed by its Wado name.
    pub interfaces: IndexMap<String, String>,
    /// Each enum's values as the host spells them, in case order, keyed by its
    /// Wado name.
    pub enums: IndexMap<String, Vec<String>>,
    /// Everything the snapshot holds that the module does not bind.
    pub skipped: Vec<Skip>,
}

/// What would have to lower for a skipped item to. A wrapper (a list of it, an
/// argument of it) rewords a reason and keeps its cause.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Cause {
    Dictionary,
    Enum,
    Namespace,
    Any,
    Object,
    /// An `ArrayBuffer`, a view of one, or a typed array.
    Buffer,
    BigInt,
    /// `undefined` anywhere but a return type.
    Undefined,
    /// A name the snapshot does not define.
    OutsideSlice,
    /// A generic other than a list or a returned `Promise`, by its name.
    Generic(String),
    /// A list whose elements are lists, nullable or callbacks.
    ListElement,
    /// A union of several expressible types, or one narrowed in a result.
    Union,
    /// Several overloads lower, and nothing merges them.
    Overloads,
    /// A callback in a result, as an event handler attribute is.
    CallbackResult,
    /// A callback the glue cannot call: one returning a value, or taking more
    /// than scalars and handles.
    Callback,
    /// A callback among more parameters than cross as core values: they cross
    /// in memory, where a closure does not.
    ParamsInMemory,
    Constant,
    /// `iterable`, `maplike`, `setlike` or `async_iterable`.
    Iteration,
    /// A getter, setter, deleter or stringifier operation.
    SpecialOperation,
    StaticAttribute,
    HtmlConstructor,
    /// A child redeclaring a method of its chain, which a resource may not.
    Override,
}

impl std::fmt::Display for Cause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = match self {
            Self::Dictionary => "a dictionary",
            Self::Enum => "an enum",
            Self::Namespace => "a namespace member",
            Self::Any => "`any`",
            Self::Object => "`object`",
            Self::Buffer => "a buffer",
            Self::BigInt => "`bigint`",
            Self::Undefined => "`undefined` outside a return type",
            Self::OutsideSlice => "a type outside the slice",
            Self::Generic(generic) => return write!(f, "`{generic}<…>`"),
            Self::ListElement => "a list of a list, a nullable type or a callback",
            Self::Union => "a union",
            Self::Overloads => "overloads",
            Self::CallbackResult => "a callback in a result",
            Self::Callback => "a callback the glue cannot call",
            Self::ParamsInMemory => "a callback among parameters passed through memory",
            Self::Constant => "a constant",
            Self::Iteration => "an iteration declaration",
            Self::SpecialOperation => "a special operation",
            Self::StaticAttribute => "a static attribute",
            Self::HtmlConstructor => "`[HTMLConstructor]`",
            Self::Override => "a redeclared inherited method",
        };
        f.write_str(label)
    }
}

/// Why something has no binding.
#[derive(Debug, Clone)]
pub struct Reason {
    pub cause: Cause,
    pub text: String,
}

impl Reason {
    fn new(cause: Cause, text: impl Into<String>) -> Self {
        Self {
            cause,
            text: text.into(),
        }
    }

    /// The reason whose text is its cause's label.
    fn of(cause: Cause) -> Self {
        let text = cause.to_string();
        Self { cause, text }
    }

    fn reworded(self, reword: impl FnOnce(&str) -> String) -> Self {
        Self {
            text: reword(&self.text),
            cause: self.cause,
        }
    }
}

/// How much of the platform a skip takes away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lost {
    /// A dictionary or an enum.
    Definition,
    Member,
    /// One overload of a member another overload binds.
    Overload,
    /// A trailing optional argument of a bound member, and each one after it.
    Argument,
}

/// One thing the snapshot holds and the module does not bind.
#[derive(Debug)]
pub struct Skip {
    /// `Interface.member`, or a definition's name.
    pub item: String,
    pub lost: Lost,
    pub reason: Reason,
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.item, self.reason.text)
    }
}

/// The skips counted by cause and by what each lost, most frequent cause first.
#[must_use]
pub fn skip_table(skipped: &[Skip]) -> String {
    const COLUMNS: [(&str, Lost); 4] = [
        ("Definitions", Lost::Definition),
        ("Members", Lost::Member),
        ("Overloads", Lost::Overload),
        ("Arguments", Lost::Argument),
    ];
    let mut counts: IndexMap<&Cause, [usize; COLUMNS.len()]> = IndexMap::new();
    let mut total = [0; COLUMNS.len()];
    for skip in skipped {
        let column = COLUMNS
            .iter()
            .position(|(_, lost)| *lost == skip.lost)
            .expect("every `Lost` has a column");
        counts.entry(&skip.reason.cause).or_default()[column] += 1;
        total[column] += 1;
    }
    // The sort is stable, so causes of one count stay in source order.
    counts.sort_by(|_, a, _, b| b.iter().sum::<usize>().cmp(&a.iter().sum()));
    let mut rows: Vec<(String, [usize; COLUMNS.len()])> = counts
        .into_iter()
        .map(|(cause, counts)| (cause.to_string(), counts))
        .collect();
    rows.push(("Total".to_string(), total));
    let width = rows
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .expect("the table has a total row");
    let mut table = format!("{:<width$}", "Cause");
    for (header, _) in COLUMNS {
        table.push_str("  ");
        table.push_str(header);
    }
    table.push('\n');
    for (label, counts) in rows {
        table.push_str(&format!("{label:<width$}"));
        for ((header, _), count) in COLUMNS.iter().zip(counts) {
            table.push_str(&format!("  {count:>w$}", w = header.len()));
        }
        table.push('\n');
    }
    table
}

/// A function's JavaScript half.
#[derive(Debug)]
pub struct JsFunction {
    pub member: JsMember,
    /// The last parameter is a variadic argument's `List`, spread into the call.
    pub variadic: bool,
}

impl From<JsMember> for JsFunction {
    fn from(member: JsMember) -> Self {
        Self {
            member,
            variadic: false,
        }
    }
}

/// What a function does with the JavaScript object it reaches. `Get`, `Set` and
/// `Call` act on the receiver; the interface names are `WebIDL`'s.
#[derive(Debug, Clone, PartialEq)]
pub enum JsMember {
    Get(String),
    Set(String),
    Call(String),
    Static {
        interface: String,
        name: String,
    },
    New {
        interface: String,
    },
    /// The `[Global]` object itself.
    Global,
    /// An attribute of the `[Global]` object.
    GlobalGet(String),
    /// The `name` of a rejection's reason, or `Error` where it has none.
    ReasonName,
    /// The `message` of a rejection's reason, or the reason as a string.
    ReasonMessage,
}

/// One interface with its partials and mixins folded in. `defined` is false
/// while only partials have been seen, which the slice does not admit.
#[derive(Default)]
struct Merged<'a> {
    defined: bool,
    inheritance: Option<String>,
    global: bool,
    members: Vec<&'a Member>,
}

/// A function, its JavaScript half, and why its trailing optional arguments
/// were left out, where they were.
struct Bound {
    function: WadoFunction,
    js: JsFunction,
    dropped: Option<Reason>,
}

/// A member's lowering, or the Wado name it would have had and why there is none.
type Lowered = std::result::Result<Bound, (String, Reason)>;

/// A callback's return type and arguments, or why no function stands in for it.
type Signature<'a> = std::result::Result<(&'a IdlType, &'a [Argument]), Reason>;

impl Callback {
    fn signature(&self) -> (&str, Signature<'_>) {
        match self {
            Self::Function {
                name,
                idl_type,
                arguments,
            } => (name, Ok((idl_type, arguments))),
            Self::Interface { name, members } => {
                let operations: Vec<_> = members
                    .iter()
                    .filter_map(|member| match member {
                        Member::Operation {
                            idl_type: Some(idl_type),
                            arguments,
                            ..
                        } => Some((idl_type, arguments.as_slice())),
                        _ => None,
                    })
                    .collect();
                let signature = match operations[..] {
                    [operation] => Ok(operation),
                    _ => Err(Reason::new(
                        Cause::Callback,
                        format!("callback interface of {} operations", operations.len()),
                    )),
                };
                (name, signature)
            }
        }
    }
}

/// The module binding the slice's interfaces and its glue, each naming
/// `source` in its header.
///
/// # Errors
///
/// See [`transform`].
pub fn generate(snapshot: &Snapshot, source: &str) -> Result<Generated> {
    let mut output = transform(snapshot)?;
    output.module.source_files = vec![source.to_string()];
    Ok(Generated {
        wado: WadoCodeGenerator::new().generate(&output.module),
        glue: glue::generate(&output, source),
        bound: output.js.len(),
        skipped: output.skipped,
    })
}

/// A package's generated files, and what the snapshot holds beyond them.
pub struct Generated {
    /// The Wado module declaring the slice's imports.
    pub wado: String,
    /// The JavaScript module serving them from the browser's objects.
    pub glue: String,
    /// How many functions the module binds.
    pub bound: usize,
    pub skipped: Vec<Skip>,
}

/// Transform a snapshot into the module binding the slice's interfaces.
///
/// # Errors
///
/// The slice is not closed (a parent or a mixin it names is missing), or it
/// holds two `[Global]` interfaces.
pub fn transform(snapshot: &Snapshot) -> Result<WebIdlOutput> {
    let merged = merge(snapshot)?;
    let lowering = Lowering {
        package: &snapshot.package,
        slice: merged.keys().copied().collect(),
        dictionaries: snapshot
            .dictionaries
            .iter()
            .fold(IndexMap::new(), |mut parts, d| {
                parts.entry(d.name.as_str()).or_default().push(d);
                parts
            }),
        lowered_dictionaries: RefCell::default(),
        dropped_fields: RefCell::default(),
        enums: snapshot
            .enums
            .iter()
            .map(|e| (e.name.as_str(), e.cases()))
            .collect(),
        typedefs: snapshot
            .typedefs
            .iter()
            .map(|t| (t.name.as_str(), &t.idl_type))
            .collect(),
        callbacks: snapshot.callbacks.iter().map(Callback::signature).collect(),
        global_interface: merged
            .values()
            .any(|iface| iface.global)
            .then(|| to_upper_camel_case(&snapshot.module)),
    };

    let classes = number_classes(&merged)?;
    let mut skipped = lowering.unbound_definitions(snapshot);
    let mut js = IndexMap::new();
    let mut resources: IndexMap<&str, WadoResource> = IndexMap::new();
    for (name, iface) in &merged {
        let path = lowering.interface_path(name);
        let methods = split_js(
            lowering.methods_of(name, &path, iface, &mut skipped),
            &mut js,
        );
        resources.insert(
            name,
            WadoResource {
                name: to_upper_camel_case(name),
                doc_comment: None,
                cm_attr: path,
                unrestricted: true,
                classes: Some(classes[name]),
                extends: iface.inheritance.as_deref().map(to_upper_camel_case),
                methods,
                unavailable: Vec::new(),
            },
        );
    }
    settle_overrides(&merged, &mut resources, &mut js, &mut skipped);

    let mut module = WadoModule::new(snapshot.module.clone(), snapshot.webref.clone());
    if let Some(bindings) = lowering.global_bindings(&merged, &resources)? {
        let functions = split_js(bindings, &mut js);
        module.interfaces.push(WadoInterface {
            name: lowering
                .global_interface
                .clone()
                .expect("a package with global bindings has a `[Global]` interface"),
            doc_comment: Some("The entry points, which hand out the first handle.".to_string()),
            cm_interface: lowering.interface_path(GLOBAL_INTERFACE),
            functions,
        });
    }
    let interfaces = resources
        .iter()
        .map(|(name, r)| (r.name.clone(), (*name).to_string()))
        .collect();
    module.resources = resources.into_values().collect();
    let mut methods = module.resources.iter().flat_map(|r| &r.methods);
    let mut functions = module.interfaces.iter().flat_map(|i| &i.functions);
    let settles = methods.any(|f| f.is_async) || functions.any(|f| f.is_async);
    if settles {
        if module.resources.iter().any(|r| r.name == REJECTION) {
            bail!("the slice declares `{REJECTION}`, which names a rejected promise's reason");
        }
        module.resources.push(lowering.rejection(&mut js));
    }
    let enums = lowering.declare_types(snapshot, &mut module);
    Ok(WebIdlOutput {
        module,
        js,
        interfaces,
        enums,
        skipped,
    })
}

/// The functions of `bindings`, their JavaScript halves moved into `js`.
fn split_js(
    bindings: Vec<(WadoFunction, JsFunction)>,
    js: &mut IndexMap<String, JsFunction>,
) -> Vec<WadoFunction> {
    bindings
        .into_iter()
        .map(|(function, member)| {
            js.insert(function.cm_attr.clone(), member);
            function
        })
        .collect()
}

/// Each interface's handle classes: its own, then its descendants' right after.
fn number_classes<'a>(
    merged: &IndexMap<&'a str, Merged<'_>>,
) -> Result<IndexMap<&'a str, HandleClasses>> {
    fn visit<'a>(
        name: &'a str,
        children: &IndexMap<&'a str, Vec<&'a str>>,
        next: &mut u16,
        out: &mut IndexMap<&'a str, HandleClasses>,
    ) -> Result<()> {
        let own = *next;
        *next = next
            .checked_add(1)
            .ok_or_else(|| anyhow!("the slice holds more interfaces than a class number counts"))?;
        for child in children.get(name).into_iter().flatten() {
            visit(child, children, next, out)?;
        }
        out.insert(
            name,
            HandleClasses {
                lo: own,
                hi: *next - 1,
            },
        );
        Ok(())
    }

    let mut children: IndexMap<&str, Vec<&str>> = IndexMap::new();
    let mut roots = Vec::new();
    for (&name, iface) in merged {
        match &iface.inheritance {
            Some(parent) => {
                let (&parent, _) = merged
                    .get_key_value(parent.as_str())
                    .expect("`merge` admits only a parent in the slice");
                children.entry(parent).or_default().push(name);
            }
            None => roots.push(name),
        }
    }
    let mut next = 0;
    let mut out = IndexMap::new();
    for root in roots {
        visit(root, &children, &mut next, &mut out)?;
    }
    let cycle: Vec<&str> = merged
        .keys()
        .copied()
        .filter(|name| !out.contains_key(name))
        .collect();
    if !cycle.is_empty() {
        bail!(
            "`{}` inherit from each other in a cycle",
            cycle.join("`, `")
        );
    }
    Ok(out)
}

/// Fold partials and mixins into their interface, in slice order.
fn merge(snapshot: &Snapshot) -> Result<IndexMap<&str, Merged<'_>>> {
    let mut merged: IndexMap<&str, Merged<'_>> = snapshot
        .slice
        .iter()
        .map(|name| (name.as_str(), Merged::default()))
        .collect();
    for iface in &snapshot.interfaces {
        let Some(target) = merged.get_mut(iface.name.as_str()) else {
            bail!("interface `{}` is not in the slice", iface.name);
        };
        if !iface.partial {
            target.defined = true;
            target.inheritance.clone_from(&iface.inheritance);
            target.global = iface.ext_attrs.iter().any(|a| a.name == "Global");
        }
        target.members.extend(&iface.members);
    }
    for (name, iface) in &merged {
        if !iface.defined {
            bail!("the slice names `{name}`, which no interface definition declares");
        }
        if let Some(parent) = &iface.inheritance
            && !merged.contains_key(parent.as_str())
        {
            bail!("`{name}` extends `{parent}`, which is not in the slice: add it");
        }
    }
    for inc in &snapshot.includes {
        let Some(target) = merged.get_mut(inc.target.as_str()) else {
            bail!("`{}` is not in the slice", inc.target);
        };
        let parts: Vec<&Interface> = snapshot
            .mixins
            .iter()
            .filter(|m| m.name == inc.includes)
            .collect();
        if !parts.iter().any(|m| !m.partial) {
            bail!(
                "`{}` includes `{}`, which the snapshot does not define",
                inc.target,
                inc.includes
            );
        }
        for mixin in parts {
            target.members.extend(&mixin.members);
        }
    }
    Ok(merged)
}

/// Settle each method a child redeclares from its chain, which a resource may
/// not. Where the nearest declaration above serves the child, the child's is
/// dropped: it has the same types, or differs only in a result that is a
/// resource extending the parent's, which a type pattern narrows. Otherwise the
/// parent's would answer with the wrong type, and the child's binds under its
/// interface's name (`svg_element_class_name`), as does a setter restored
/// below a read-only redeclaration. Either way the child declares unavailable
/// every name the chain above binds that member under, and so does a child
/// redeclaring an attribute read-only, for its setter. Statics (`new` above
/// all) are not inherited.
fn settle_overrides<'a>(
    merged: &IndexMap<&'a str, Merged<'_>>,
    resources: &mut IndexMap<&'a str, WadoResource>,
    js: &mut IndexMap<String, JsFunction>,
    skipped: &mut Vec<Skip>,
) {
    let has_receiver = |m: &WadoFunction| m.params.first().is_some_and(|p| p.wit_name == "self");
    let ancestors = |name: &'a str| {
        std::iter::successors(merged[name].inheritance.as_deref(), |parent| {
            merged[*parent].inheritance.as_deref()
        })
    };
    let interface_of: IndexMap<String, &str> = resources
        .iter()
        .map(|(name, r)| (r.name.clone(), *name))
        .collect();
    let serves = |child: &WadoFunction, parent: &WadoFunction| {
        let types = |params: &[WadoParam]| -> Vec<WadoType> {
            params[1..].iter().map(|p| p.ty.clone()).collect()
        };
        types(&child.params) == types(&parent.params)
            && child.is_async == parent.is_async
            && match (&child.return_type, &parent.return_type) {
                (Some(child), Some(parent)) => narrows(child, parent, |c, p| {
                    ancestors(interface_of[c]).any(|a| a == interface_of[p])
                }),
                (None, None) => true,
                (None, Some(_)) | (Some(_), None) => false,
            }
    };
    let declared: IndexMap<&str, IndexMap<String, WadoFunction>> = resources
        .iter()
        .map(|(name, r)| {
            let receivers = r.methods.iter().filter(|m| has_receiver(m));
            (
                *name,
                receivers.map(|m| (m.name.clone(), m.clone())).collect(),
            )
        })
        .collect();
    // The JavaScript members each interface takes from the bindings above it,
    // with the reason a call of one reports.
    let mut taken: IndexMap<&str, Vec<(JsMember, String)>> = IndexMap::new();
    for (name, iface) in merged {
        let taken = taken.entry(*name).or_default();
        // An attribute is one accessor in JavaScript, so redeclared read-only
        // it leaves no setter: the write would throw.
        taken.extend(read_only_attributes(iface).map(|attribute| {
            let reason = format!("`{name}` redeclares `{attribute}` as read-only");
            (JsMember::Set(attribute.to_string()), reason)
        }));
        resources[name].methods.retain_mut(|method| {
            if !has_receiver(method) {
                return true;
            }
            let Some((parent, inherited)) = ancestors(name)
                .find_map(|parent| Some((parent, declared[parent].get(&method.name)?)))
            else {
                return true;
            };
            let member = &js[&method.cm_attr].member;
            let hides_setter = |between: &&str| {
                matches!(member, JsMember::Set(attribute)
                    if read_only_attributes(&merged[*between]).any(|a| a == attribute))
            };
            let why = if serves(method, inherited) {
                ancestors(name)
                    .take_while(|between| *between != parent)
                    .find(hides_setter)
                    .map(|between| format!("restores the setter `{between}` hides"))
            } else {
                Some("redeclares it with another type".to_string())
            };
            if let Some(why) = why {
                let interface = to_snake_case(name);
                let own = match member {
                    JsMember::Set(_) => {
                        method
                            .name
                            .replacen("set_", &format!("set_{interface}_"), 1)
                    }
                    _ => format!("{interface}_{}", method.name),
                };
                assert!(
                    !declared[name].contains_key(&own),
                    "`{name}` must not declare `{own}` beside the `{}` it redeclares",
                    method.name
                );
                taken.push((member.clone(), format!("`{name}` {why}; use `{own}`")));
                method.name = own;
                return true;
            }
            js.shift_remove(&method.cm_attr)
                .expect("every function has a JavaScript half");
            skipped.push(Skip {
                item: format!("{name}.{}", method.name),
                lost: Lost::Member,
                reason: Reason::new(
                    Cause::Override,
                    format!("redeclares a method inherited from `{parent}`"),
                ),
            });
            false
        });
    }
    let js = &*js;
    for (name, taken) in taken {
        let hidden: Vec<(String, String)> = taken
            .iter()
            .flat_map(|(member, reason)| {
                ancestors(name)
                    .flat_map(|above| &resources[above].methods)
                    .filter(move |bound| js[&bound.cm_attr].member == *member)
                    .map(move |bound| (bound.name.clone(), reason.clone()))
            })
            .collect();
        resources[name].unavailable = hidden;
    }
}

/// The attributes an interface declares read-only on its instances.
fn read_only_attributes<'m>(iface: &'m Merged<'_>) -> impl Iterator<Item = &'m str> {
    iface.members.iter().filter_map(|member| match member {
        Member::Attribute {
            name,
            readonly: true,
            special,
            ..
        } if special != "static" => Some(name.as_str()),
        _ => None,
    })
}

/// Whether `child` is `parent`, or `parent` with a resource replaced by one
/// that `extends` it.
fn narrows(child: &WadoType, parent: &WadoType, extends: impl Fn(&str, &str) -> bool) -> bool {
    match (child, parent) {
        (WadoType::Option(child), WadoType::Option(parent)) => narrows(child, parent, extends),
        (WadoType::Named(child), WadoType::Named(parent)) => {
            child == parent || extends(child, parent)
        }
        _ => child == parent,
    }
}

/// Each enum's Wado cases, or why it has none, by its `WebIDL` name.
type Enums<'a> = IndexMap<&'a str, std::result::Result<Vec<String>, Reason>>;

type LoweredDictionary = std::result::Result<WadoDictionary, Reason>;

struct Lowering<'a> {
    package: &'a str,
    slice: IndexSet<&'a str>,
    /// Each dictionary's definition and partials, by name.
    dictionaries: IndexMap<&'a str, Vec<&'a Dictionary>>,
    /// Each dictionary lowered so far; `None` while its members are lowered.
    lowered_dictionaries: RefCell<IndexMap<&'a str, Option<LoweredDictionary>>>,
    /// The members a lowered dictionary leaves to the host's default.
    dropped_fields: RefCell<Vec<Skip>>,
    enums: Enums<'a>,
    typedefs: IndexMap<&'a str, &'a IdlType>,
    callbacks: IndexMap<&'a str, Signature<'a>>,
    /// The interface handing out the first handle, which a callback's body may
    /// perform. `None` without a `[Global]` interface.
    global_interface: Option<String>,
}

impl Lowering<'_> {
    fn interface_path(&self, name: &str) -> String {
        format!("{}/{}", self.package, to_kebab_case(name))
    }

    /// The definitions nothing stands for: each dictionary and enum that does
    /// not lower, each dictionary member left out, and each member of a
    /// namespace.
    fn unbound_definitions(&self, snapshot: &Snapshot) -> Vec<Skip> {
        let definition = |name: &str, reason: Reason| Skip {
            item: name.to_string(),
            lost: Lost::Definition,
            reason,
        };
        let mut skipped = Vec::new();
        for name in self.dictionaries.keys() {
            let lowered = self.lower_dictionary(name);
            skipped.append(&mut self.dropped_fields.borrow_mut());
            if let Err(reason) = lowered {
                skipped.push(definition(name, reason));
            }
        }
        for (name, cases) in &self.enums {
            if let Err(reason) = cases {
                skipped.push(definition(name, reason.clone()));
            }
        }
        for namespace in &snapshot.namespaces {
            skipped.extend(namespace.members.iter().map(|member| Skip {
                item: format!("{}.{}", namespace.name, member.label()),
                lost: Lost::Member,
                reason: Reason::of(Cause::Namespace),
            }));
        }
        skipped
    }

    /// The record the dictionary `name` crosses as: its parent's members, then
    /// its own. A member is as the host may answer it, since one struct serves
    /// both ways. One that does not lower is left to the host's default,
    /// unless it is required.
    fn lower_dictionary(&self, name: &str) -> LoweredDictionary {
        let (&name, parts) = self
            .dictionaries
            .get_key_value(name)
            .expect("a dictionary of the snapshot");
        if let Some(lowered) = self.lowered_dictionaries.borrow().get(name) {
            return lowered.clone().unwrap_or_else(|| {
                Err(Reason::new(
                    Cause::Dictionary,
                    format!("`{name}` holds itself"),
                ))
            });
        }
        self.lowered_dictionaries.borrow_mut().insert(name, None);
        let lowered = self.lower_dictionary_parts(name, parts);
        self.lowered_dictionaries
            .borrow_mut()
            .insert(name, Some(lowered.clone()));
        lowered
    }

    fn lower_dictionary_parts(&self, name: &str, parts: &[&Dictionary]) -> LoweredDictionary {
        let mut fields = Vec::new();
        for parent in parts.iter().filter_map(|part| part.inheritance.as_deref()) {
            let parent = self
                .lower_dictionary(parent)
                .map_err(|reason| reason.reworded(|r| format!("its parent `{parent}`: {r}")))?;
            fields.extend(parent.fields);
        }
        for field in parts.iter().flat_map(|part| &part.members) {
            match self.lower_type(&field.idl_type, Flow::Out) {
                Ok(ty) => fields.push(DictionaryField {
                    host_name: field.name.clone(),
                    ty: optional(ty, !field.required),
                }),
                Err(reason) if field.required => {
                    return Err(reason.reworded(|r| format!("`{}`: {r}", field.name)));
                }
                Err(reason) => self.dropped_fields.borrow_mut().push(Skip {
                    item: format!("{name}.{}", field.name),
                    lost: Lost::Member,
                    reason,
                }),
            }
        }
        if fields.is_empty() {
            return Err(Reason::new(
                Cause::Dictionary,
                "a dictionary without a member that binds",
            ));
        }
        Ok(WadoDictionary {
            name: to_upper_camel_case(name),
            fields,
        })
    }

    /// Declares in `module` each enum and dictionary a function of it names,
    /// and answers each enum's values as the host spells them, in case order,
    /// by Wado name.
    fn declare_types(
        &self,
        snapshot: &Snapshot,
        module: &mut WadoModule,
    ) -> IndexMap<String, Vec<String>> {
        let mut enums = IndexSet::new();
        let mut dictionaries = IndexMap::new();
        let mut unions = IndexMap::new();
        let methods = module.resources.iter().flat_map(|r| &r.methods);
        let functions = module.interfaces.iter().flat_map(|i| &i.functions);
        for function in methods.chain(functions) {
            let params = function.params.iter().map(|p| &p.ty);
            for ty in params.chain(&function.return_type) {
                ty.walk(&mut |ty| match ty {
                    WadoType::StringEnum(name) => {
                        enums.insert(name.clone());
                    }
                    WadoType::Dictionary(dictionary) => {
                        dictionaries.insert(dictionary.name.clone(), dictionary.clone());
                    }
                    WadoType::Union(union) => {
                        let declared = unions.insert(union.name.clone(), union.clone());
                        assert!(
                            declared.is_none_or(|declared| declared == *union),
                            "two unions are named `{}`",
                            union.name
                        );
                    }
                    _ => {}
                });
            }
        }
        let type_path =
            |name: &str| format!("{}/{TYPES_INTERFACE}#{}", self.package, to_kebab_case(name));
        let mut values = IndexMap::new();
        for declared in &snapshot.enums {
            let name = to_upper_camel_case(&declared.name);
            if !enums.contains(&name) {
                continue;
            }
            let cases = self.enums[declared.name.as_str()]
                .as_ref()
                .expect("a function names only an enum that has cases");
            module.types.push(WadoTypeDef::Enum(WadoEnum {
                name: name.clone(),
                doc_comment: None,
                cm_attr: Some(type_path(&declared.name)),
                variants: cases
                    .iter()
                    .map(|case| WadoEnumVariant {
                        name: case.clone(),
                        doc_comment: None,
                        cm_attr: None,
                    })
                    .collect(),
            }));
            values.insert(
                name,
                declared.values.iter().map(|v| v.value.clone()).collect(),
            );
        }
        for declared in self.dictionaries.keys() {
            let Some(dictionary) = dictionaries.get(&to_upper_camel_case(declared)) else {
                continue;
            };
            let fields = dictionary
                .fields
                .iter()
                .map(|field| WadoField {
                    name: to_wado_identifier(&field.host_name),
                    doc_comment: None,
                    cm_attr: to_kebab_case(&field.host_name),
                    default: matches!(field.ty, WadoType::Option(_)).then(|| "null".to_string()),
                    ty: field.ty.clone(),
                })
                .collect();
            module.types.push(WadoTypeDef::Struct(WadoStruct {
                name: dictionary.name.clone(),
                doc_comment: None,
                cm_attr: Some(type_path(declared)),
                is_affine: false,
                fields,
            }));
        }
        unions.sort_keys();
        for union in unions.into_values() {
            module.types.push(WadoTypeDef::Variant(WadoVariant {
                cm_attr: Some(type_path(&union.name)),
                name: union.name,
                doc_comment: None,
                is_affine: false,
                cases: union
                    .cases
                    .into_iter()
                    .map(|case| WadoVariantCase {
                        name: case.name,
                        payload: Some(case.ty),
                        doc_comment: None,
                        cm_attr: None,
                    })
                    .collect(),
            }));
        }
        values
    }

    /// The resource a rejected promise's reason is. It carries no class: a
    /// reason is narrowed to nothing.
    fn rejection(&self, js: &mut IndexMap<String, JsFunction>) -> WadoResource {
        let path = self.interface_path(REJECTION);
        let reads = |name: &str, member: JsMember| {
            let reader = function(
                name.to_string(),
                format!("{path}#{name}"),
                vec![self_param(REJECTION)],
                Some(WadoType::String),
            );
            (reader, member.into())
        };
        let methods = split_js(
            vec![
                reads("name", JsMember::ReasonName),
                reads("message", JsMember::ReasonMessage),
            ],
            js,
        );
        WadoResource {
            name: REJECTION.to_string(),
            doc_comment: Some(
                "Why a promise was rejected: any value, which need not be an `Error`.".to_string(),
            ),
            cm_attr: path,
            unrestricted: true,
            classes: None,
            extends: None,
            methods,
            unavailable: Vec::new(),
        }
    }

    /// Every member of `iface` that lowers, appending to `skipped` the ones
    /// that do not, the names two overloads compete for, and what a bound
    /// member leaves out.
    fn methods_of(
        &self,
        iface: &str,
        path: &str,
        merged: &Merged<'_>,
        skipped: &mut Vec<Skip>,
    ) -> Vec<(WadoFunction, JsFunction)> {
        let mut candidates: IndexMap<String, (Vec<Bound>, Vec<Reason>)> = IndexMap::new();
        for member in &merged.members {
            for lowered in self.lower_member(iface, path, member) {
                match lowered {
                    Ok(bound) => {
                        let name = bound.function.name.clone();
                        candidates.entry(name).or_default().0.push(bound);
                    }
                    Err((name, reason)) => candidates.entry(name).or_default().1.push(reason),
                }
            }
        }
        let mut methods = Vec::new();
        for (name, (mut bound, reasons)) in candidates {
            let item = format!("{iface}.{name}");
            let mut skip = |lost, reason| {
                skipped.push(Skip {
                    item: item.clone(),
                    lost,
                    reason,
                });
            };
            match bound.len() {
                1 => {
                    let Bound {
                        function,
                        js,
                        dropped,
                    } = bound.pop().unwrap();
                    methods.push((function, js));
                    for reason in reasons {
                        skip(Lost::Overload, reason);
                    }
                    if let Some(reason) = dropped {
                        skip(Lost::Argument, reason);
                    }
                }
                0 => {
                    for reason in reasons {
                        skip(Lost::Member, reason);
                    }
                }
                n => skip(
                    Lost::Member,
                    Reason::new(Cause::Overloads, format!("{n} overloads")),
                ),
            }
        }
        methods
    }

    /// The functions a member yields: a getter and a setter for an attribute,
    /// one function otherwise. `path` is `iface`'s CM interface.
    fn lower_member(&self, iface: &str, path: &str, member: &Member) -> Vec<Lowered> {
        match member {
            Member::Attribute {
                name,
                idl_type,
                readonly,
                special,
            } => {
                let getter = to_wado_identifier(name);
                if special == "static" {
                    let reason = Reason::new(Cause::StaticAttribute, "static attribute");
                    return vec![Err((getter, reason))];
                }
                let kebab = to_kebab_case(name);
                let mut out = vec![match self.lower_type(idl_type, Flow::Out) {
                    Ok(ty) => Bound::whole(
                        function(
                            getter,
                            format!("{path}#{kebab}"),
                            vec![self_param(iface)],
                            Some(ty),
                        ),
                        JsMember::Get(name.clone()),
                    ),
                    Err(reason) => Err((getter, reason)),
                }];
                // `Flow::In` admits everything `Flow::Out` does, so a setter
                // that fails here fails for the reason the getter just gave.
                if let Some(Ok(ty)) = (!readonly).then(|| self.lower_type(idl_type, Flow::In)) {
                    let value = WadoParam {
                        name: "value".to_string(),
                        ty,
                        wit_name: "value".to_string(),
                        default: None,
                        lends_view: false,
                    };
                    out.push(Bound::whole(
                        function(
                            format!("set_{}", to_snake_case(name)),
                            format!("{path}#set-{kebab}"),
                            vec![self_param(iface), value],
                            None,
                        ),
                        JsMember::Set(name.clone()),
                    ));
                }
                out
            }
            Member::Operation {
                name,
                idl_type,
                arguments,
                special,
            } => {
                let wado_name = if name.is_empty() {
                    format!("({special})")
                } else {
                    to_wado_identifier(name)
                };
                let (receiver, js) = match special.as_str() {
                    "" => (Some(self_param(iface)), JsMember::Call(name.clone())),
                    "static" => (
                        None,
                        JsMember::Static {
                            interface: iface.to_string(),
                            name: name.clone(),
                        },
                    ),
                    _ => {
                        let reason =
                            Reason::new(Cause::SpecialOperation, format!("{special} operation"));
                        return vec![Err((wado_name, reason))];
                    }
                };
                let idl_type = idl_type
                    .as_ref()
                    .expect("only a bare `stringifier;` has no return type");
                vec![self.lower_operation(
                    iface,
                    (wado_name, js),
                    format!("{path}#{}", to_kebab_case(name)),
                    receiver,
                    arguments,
                    Some(idl_type),
                )]
            }
            Member::Constructor {
                arguments,
                ext_attrs,
            } => {
                // `[HTMLConstructor]` runs only from a custom element definition.
                if ext_attrs.iter().any(|a| a.name == "HTMLConstructor") {
                    let reason = Reason::new(Cause::HtmlConstructor, "HTMLConstructor");
                    return vec![Err(("new".to_string(), reason))];
                }
                let js = JsMember::New {
                    interface: iface.to_string(),
                };
                vec![self.lower_operation(
                    iface,
                    ("new".to_string(), js),
                    format!("{path}#new"),
                    None,
                    arguments,
                    None,
                )]
            }
            Member::Const { .. } => vec![Err((member.label(), Reason::of(Cause::Constant)))],
            Member::Iterable | Member::Maplike | Member::Setlike | Member::AsyncIterable => {
                vec![Err((member.label(), Reason::of(Cause::Iteration)))]
            }
        }
    }

    /// `return_type` is `None` for a constructor, which yields `iface`.
    fn lower_operation(
        &self,
        iface: &str,
        (wado_name, member): (String, JsMember),
        cm_attr: String,
        receiver: Option<WadoParam>,
        arguments: &[Argument],
        return_type: Option<&IdlType>,
    ) -> Lowered {
        let skip = |reason: Reason| Err((wado_name.clone(), reason));
        // A `Promise<T>` result is an `async` function settling to `T` or to
        // the reason it was rejected for.
        let settled = return_type.and_then(promised);
        let answered = match settled.or(return_type) {
            None => Some(WadoType::Named(to_upper_camel_case(iface))),
            Some(ty) if is_undefined(ty) => None,
            Some(ty) => match self.lower_type(ty, Flow::Out) {
                Ok(ty) => Some(ty),
                Err(reason) => return skip(reason),
            },
        };
        let is_async = settled.is_some();
        let return_type = if is_async {
            Some(WadoType::Result {
                ok: answered.map(Box::new),
                err: Some(Box::new(WadoType::Named(REJECTION.to_string()))),
            })
        } else {
            answered
        };
        let mut params: Vec<WadoParam> = receiver.into_iter().collect();
        let receivers = params.len();
        let mut variadic = false;
        let mut dropped = None;
        for arg in arguments {
            assert!(!variadic, "WebIDL admits a variadic argument only last");
            let (ty, default) = match self.lower_type(&arg.idl_type, Flow::In) {
                // An empty list leaves the argument out.
                Ok(ty) if arg.variadic => {
                    variadic = true;
                    (WadoType::List(Box::new(ty)), None)
                }
                Ok(ty) if arg.optional => optional_argument(ty, arg.default.as_ref()),
                Ok(ty) => (ty, None),
                // A trailing optional the slice cannot express is left to
                // its WebIDL default; a required one takes the member with it.
                Err(reason) if arg.optional => {
                    dropped =
                        Some(reason.reworded(|r| format!("optional `{}` dropped: {r}", arg.name)));
                    break;
                }
                Err(reason) => {
                    return skip(reason.reworded(|r| format!("`{}`: {r}", arg.name)));
                }
            };
            params.push(WadoParam {
                name: to_wado_identifier(&arg.name),
                ty,
                wit_name: to_kebab_case(&arg.name),
                default,
                lends_view: false,
            });
        }
        // Wado takes defaults only on trailing parameters.
        if let Some(last_required) = params.iter().rposition(|p| p.default.is_none()) {
            for param in &mut params[..last_required] {
                param.default = None;
            }
        }
        // No shim reads a closure from memory, so trailing optionals are left
        // to their WebIDL defaults until the rest cross as core values.
        while !glue::reads_params(&params) {
            if params.pop().is_none_or(|last| last.default.is_none()) {
                return skip(Reason::of(Cause::ParamsInMemory));
            }
            let left_out = &arguments[params.len() - receivers].name;
            dropped = Some(
                Reason::of(Cause::ParamsInMemory)
                    .reworded(|r| format!("optional `{left_out}` dropped: {r}")),
            );
        }
        let function = WadoFunction {
            is_async,
            ..function(wado_name, cm_attr, params, return_type)
        };
        Ok(Bound {
            function,
            js: JsFunction { member, variadic },
            dropped,
        })
    }

    /// The Wado type of a `WebIDL` type, or why the slice has none. A union is
    /// the one constituent the slice can express; `undefined` in it means nullable.
    fn lower_type(&self, ty: &IdlType, flow: Flow) -> std::result::Result<WadoType, Reason> {
        let (inner, nullable) = match &ty.inner {
            IdlTypeInner::Types(arguments) if !ty.generic.is_empty() => (
                self.lower_generic(&ty.generic, arguments, flow)?,
                ty.nullable,
            ),
            IdlTypeInner::Name(_) if !ty.generic.is_empty() => {
                unreachable!("webidl2 lists a generic's arguments")
            }
            IdlTypeInner::Name(name) => (self.lower_name(name, flow)?, ty.nullable),
            IdlTypeInner::Types(constituents) => {
                let mut expressible = Vec::new();
                let mut reasons = Vec::new();
                let mut nullable = ty.nullable || constituents.iter().any(is_undefined);
                for constituent in constituents.iter().filter(|c| !is_undefined(c)) {
                    let lowered = match self.lower_type(constituent, flow) {
                        Ok(lowered) => lowered,
                        Err(reason) => {
                            reasons.push(reason);
                            continue;
                        }
                    };
                    // A nullable constituent makes the union nullable, and a
                    // union in a union gives it its constituents.
                    let lowered = match lowered {
                        WadoType::Option(inner) => {
                            nullable = true;
                            *inner
                        }
                        lowered => lowered,
                    };
                    let cases = match lowered {
                        WadoType::Union(union) => union.cases.into_iter().map(|c| c.ty).collect(),
                        lowered => vec![lowered],
                    };
                    for case in cases {
                        if !expressible.contains(&case) {
                            expressible.push(case);
                        }
                    }
                }
                let texts = || {
                    let texts: Vec<&str> = reasons.iter().map(|r| r.text.as_str()).collect();
                    texts.join("; ")
                };
                match expressible.len() {
                    1 if reasons.is_empty() || flow == Flow::In => (),
                    1 => {
                        return Err(Reason::new(
                            Cause::Union,
                            format!("union narrowed in a result: {}", texts()),
                        ));
                    }
                    // Its first constituent has to lower before the union can.
                    0 => {
                        return Err(Reason::new(
                            reasons[0].cause.clone(),
                            format!("union: {}", texts()),
                        ));
                    }
                    n if flow == Flow::Out => {
                        return Err(Reason::new(
                            Cause::Union,
                            format!("union of {n} expressible types in a result"),
                        ));
                    }
                    _ => return Ok(optional(union_of(expressible)?, nullable)),
                }
                (expressible.pop().unwrap(), nullable)
            }
        };
        Ok(optional(inner, nullable))
    }

    /// A `sequence<T>` or a `FrozenArray<T>` is a `List<T>` of what a list
    /// carries across: a scalar, a string or a handle.
    fn lower_generic(
        &self,
        generic: &str,
        arguments: &[IdlType],
        flow: Flow,
    ) -> std::result::Result<WadoType, Reason> {
        let ("sequence" | "FrozenArray", [element]) = (generic, arguments) else {
            return Err(Reason::of(Cause::Generic(generic.to_string())));
        };
        let reason = |of: &str| format!("`{generic}` of {of}");
        let element = self
            .lower_type(element, flow)
            .map_err(|r| r.reworded(reason))?;
        if !glue::list_carries(&element) {
            return Err(Reason::new(
                Cause::ListElement,
                reason("a list, a nullable type or a callback"),
            ));
        }
        Ok(WadoType::List(Box::new(element)))
    }

    fn lower_name(&self, name: &str, flow: Flow) -> std::result::Result<WadoType, Reason> {
        let named = |cause: Cause| Err(Reason::new(cause, format!("`{name}`")));
        let is_a =
            |cause: Cause, what: &str| Err(Reason::new(cause, format!("`{name}` is {what}")));
        Ok(match name {
            "boolean" => WadoType::Bool,
            "byte" => WadoType::I8,
            "octet" => WadoType::U8,
            "short" => WadoType::I16,
            "unsigned short" => WadoType::U16,
            "long" => WadoType::I32,
            "unsigned long" => WadoType::U32,
            "long long" => WadoType::I64,
            "unsigned long long" => WadoType::U64,
            "float" | "unrestricted float" => WadoType::F32,
            "double" | "unrestricted double" => WadoType::F64,
            "DOMString" | "USVString" | "ByteString" => WadoType::String,
            "undefined" => return Err(Reason::of(Cause::Undefined)),
            "any" => return named(Cause::Any),
            "object" => return named(Cause::Object),
            "bigint" => return named(Cause::BigInt),
            "ArrayBuffer" | "SharedArrayBuffer" | "DataView" | "Int8Array" | "Int16Array"
            | "Int32Array" | "Uint8Array" | "Uint16Array" | "Uint32Array" | "Uint8ClampedArray"
            | "BigInt64Array" | "BigUint64Array" | "Float16Array" | "Float32Array"
            | "Float64Array" => return is_a(Cause::Buffer, "a buffer"),
            _ if self.slice.contains(name) => WadoType::Named(to_upper_camel_case(name)),
            _ if self.dictionaries.contains_key(name) => match self.lower_dictionary(name) {
                Ok(dictionary) => WadoType::Dictionary(dictionary),
                Err(_) => return is_a(Cause::Dictionary, "a dictionary without a binding"),
            },
            _ => {
                if let Some(cases) = self.enums.get(name) {
                    return match cases {
                        Ok(_) => Ok(WadoType::StringEnum(to_upper_camel_case(name))),
                        Err(_) => is_a(Cause::Enum, "an enum without a binding"),
                    };
                }
                if let Some(target) = self.typedefs.get(name) {
                    // A union takes the name its typedef gives it.
                    let named = |mut union: WadoUnion| {
                        union.name = to_upper_camel_case(name);
                        WadoType::Union(union)
                    };
                    return Ok(match self.lower_type(target, flow)? {
                        WadoType::Union(union) => named(union),
                        WadoType::Option(inner) => WadoType::Option(Box::new(match *inner {
                            WadoType::Union(union) => named(union),
                            inner => inner,
                        })),
                        lowered => lowered,
                    });
                }
                return match self.callbacks.get(name) {
                    Some(signature) => self.lower_callback(signature.clone()?, flow),
                    None => is_a(Cause::OutsideSlice, "outside the slice"),
                };
            }
        })
    }

    /// A callback crosses only into the host, returns nothing, and takes only
    /// what the host always calls a closure back with: scalars and handles.
    fn lower_callback(
        &self,
        (return_type, arguments): (&IdlType, &[Argument]),
        flow: Flow,
    ) -> std::result::Result<WadoType, Reason> {
        if flow == Flow::Out {
            return Err(Reason::of(Cause::CallbackResult));
        }
        let uncallable = |why: String| Err(Reason::new(Cause::Callback, why));
        if !is_undefined(return_type) {
            return uncallable("a callback returning a value".to_string());
        }
        let params = arguments
            .iter()
            .map(|arg| {
                let argument = |what: &str| format!("callback argument `{}`: {what}", arg.name);
                if arg.variadic {
                    return uncallable(argument("variadic"));
                }
                if arg.optional {
                    return uncallable(argument("optional"));
                }
                let ty = self.lower_type(&arg.idl_type, Flow::Out)?;
                if ty.callback_argument_word().is_some() {
                    return Ok(ty);
                }
                uncallable(argument("neither a scalar nor a handle"))
            })
            .collect::<std::result::Result<_, _>>()?;
        Ok(WadoType::Callback {
            params,
            effect: self.global_interface.clone(),
        })
    }

    /// The functions handing out the first handle: the `[Global]` interface, and
    /// each of its read-only attributes typed as another slice resource.
    fn global_bindings(
        &self,
        merged: &IndexMap<&str, Merged<'_>>,
        resources: &IndexMap<&str, WadoResource>,
    ) -> Result<Option<Vec<(WadoFunction, JsFunction)>>> {
        let mut globals = merged.iter().filter(|(_, iface)| iface.global);
        let Some((name, global)) = globals.next() else {
            return Ok(None);
        };
        if let Some((second, _)) = globals.next() {
            bail!("a package has one `[Global]` interface; the slice has `{name}` and `{second}`");
        }
        let path = self.interface_path(GLOBAL_INTERFACE);
        let accessor = |wado_name: String, kebab: &str, ty: &str| {
            function(
                wado_name,
                format!("{path}#{kebab}"),
                Vec::new(),
                Some(WadoType::Named(ty.to_string())),
            )
        };
        let global_type = to_upper_camel_case(name);
        let mut bindings = vec![(
            accessor(to_wado_identifier(name), &to_kebab_case(name), &global_type),
            JsMember::Global.into(),
        )];
        let methods: IndexSet<&str> = resources[name]
            .methods
            .iter()
            .map(|m| m.name.as_str())
            .collect();
        for member in &global.members {
            if let Member::Attribute {
                name,
                idl_type,
                readonly: true,
                ..
            } = member
                && let Ok(WadoType::Named(ty)) = self.lower_type(idl_type, Flow::Out)
                && ty != global_type
            {
                let wado_name = to_wado_identifier(name);
                if methods.contains(wado_name.as_str()) {
                    bindings.push((
                        accessor(wado_name, &to_kebab_case(name), &ty),
                        JsMember::GlobalGet(name.clone()).into(),
                    ));
                }
            }
        }
        Ok(Some(bindings))
    }
}

impl Bound {
    /// A binding that leaves no argument out.
    fn whole(function: WadoFunction, member: JsMember) -> Lowered {
        Ok(Self {
            function,
            js: member.into(),
            dropped: None,
        })
    }
}

fn function(
    name: String,
    cm_attr: String,
    params: Vec<WadoParam>,
    return_type: Option<WadoType>,
) -> WadoFunction {
    WadoFunction {
        name,
        doc_comment: None,
        cm_attr,
        params,
        return_type,
        is_async: false,
        never_returns: false,
    }
}

fn self_param(iface: &str) -> WadoParam {
    WadoParam {
        name: "self".to_string(),
        ty: WadoType::Borrow(Box::new(WadoType::Named(to_upper_camel_case(iface)))),
        wit_name: "self".to_string(),
        default: None,
        lends_view: false,
    }
}

/// The variant holding one of `types`, named after them: `NodeOrString`, whose
/// cases are `AsNode` and `AsString`.
fn union_of(types: Vec<WadoType>) -> std::result::Result<WadoType, Reason> {
    let cases = types
        .into_iter()
        .map(|ty| match union_word(&ty) {
            Some(word) => Ok((word, ty)),
            None => Err(Reason::new(
                Cause::Union,
                "union of a callback and another type",
            )),
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let words: Vec<&str> = cases.iter().map(|(word, _)| word.as_str()).collect();
    Ok(WadoType::Union(WadoUnion {
        name: words.join("Or"),
        cases: cases
            .iter()
            .map(|(word, ty)| UnionCase {
                name: format!("As{word}"),
                ty: ty.clone(),
            })
            .collect(),
    }))
}

/// What a union's name calls a constituent of type `ty`; `None` for a
/// callback, which is no part of a variant.
fn union_word(ty: &WadoType) -> Option<String> {
    match ty {
        WadoType::String => Some("String".to_string()),
        WadoType::Named(name) | WadoType::StringEnum(name) => Some(name.clone()),
        WadoType::Dictionary(dictionary) => Some(dictionary.name.clone()),
        // A union of its own only as a list's element: one in a union is its cases.
        WadoType::Union(union) => Some(union.name.clone()),
        WadoType::List(element) => Some(format!("{}List", union_word(element)?)),
        WadoType::Callback { .. } => None,
        WadoType::Bool
        | WadoType::Char
        | WadoType::I8
        | WadoType::I16
        | WadoType::I32
        | WadoType::I64
        | WadoType::I128
        | WadoType::U8
        | WadoType::U16
        | WadoType::U32
        | WadoType::U64
        | WadoType::U128
        | WadoType::F32
        | WadoType::F64 => ty.primitive_name().map(to_upper_camel_case),
        WadoType::Option(_)
        | WadoType::Result { .. }
        | WadoType::HashMap(..)
        | WadoType::Tuple(_)
        | WadoType::Stream(_)
        | WadoType::Future(_)
        | WadoType::Borrow(_) => unreachable!("a union's constituent lowers to no {ty:?}"),
    }
}

/// `ty` as an `Option` when `wrap`, unless it is one or a callback: the DOM
/// ignores a null listener, so leaving the call out says the same.
fn optional(ty: WadoType, wrap: bool) -> WadoType {
    match ty {
        WadoType::Option(_) | WadoType::Callback { .. } => ty,
        ty if wrap => WadoType::Option(Box::new(ty)),
        ty => ty,
    }
}

/// An optional argument's type and default. A `WebIDL` default `ty` can spell is
/// written out; any other is `None`, which crosses as `undefined`, so the
/// browser applies its own default. A callback has no default to write.
fn optional_argument(ty: WadoType, default: Option<&DefaultValue>) -> (WadoType, Option<String>) {
    let written = match (default, &ty) {
        (Some(DefaultValue::Boolean { value }), WadoType::Bool) => Some(value.to_string()),
        (Some(DefaultValue::Number { value }), WadoType::F32 | WadoType::F64) => {
            Some(format!("{:?}", float_value(value)))
        }
        (
            Some(DefaultValue::Number { value }),
            WadoType::I8
            | WadoType::I16
            | WadoType::I32
            | WadoType::I64
            | WadoType::I128
            | WadoType::U8
            | WadoType::U16
            | WadoType::U32
            | WadoType::U64
            | WadoType::U128,
        ) => Some(integer_value(value).to_string()),
        (Some(DefaultValue::String { value }), WadoType::String) => Some(string_literal(value)),
        (Some(DefaultValue::String { value }), WadoType::StringEnum(name)) => {
            Some(format!("{name}::{}", to_upper_camel_case(value)))
        }
        _ => None,
    };
    match (written, ty) {
        (Some(written), ty) => (ty, Some(written)),
        (None, ty @ WadoType::Callback { .. }) => (ty, None),
        (None, ty) => (optional(ty, true), Some("null".to_string())),
    }
}

/// A `WebIDL` `integer` token's value: a leading `0` is octal, unlike in Wado.
fn integer_value(token: &str) -> i128 {
    let (negative, digits) = match token.strip_prefix('-') {
        Some(digits) => (true, digits),
        None => (false, token),
    };
    let magnitude = if let Some(hex) = digits
        .strip_prefix("0x")
        .or_else(|| digits.strip_prefix("0X"))
    {
        i128::from_str_radix(hex, 16)
    } else if digits.len() > 1 && digits.starts_with('0') {
        i128::from_str_radix(&digits[1..], 8)
    } else {
        digits.parse()
    }
    .unwrap_or_else(|e| panic!("WebIDL integer `{token}`: {e}"));
    if negative { -magnitude } else { magnitude }
}

/// A `WebIDL` `float` or `integer` token's value. A float may start or end at
/// its `.`, which Wado does not admit.
fn float_value(token: &str) -> f64 {
    if token.contains(['.', 'e', 'E']) && !token.contains(['x', 'X']) {
        token
            .parse()
            .unwrap_or_else(|e| panic!("WebIDL float `{token}`: {e}"))
    } else {
        integer_value(token) as f64
    }
}

fn string_literal(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The `T` of a `Promise<T>`.
fn promised(ty: &IdlType) -> Option<&IdlType> {
    match &ty.inner {
        IdlTypeInner::Types(arguments) if ty.generic == "Promise" => match &arguments[..] {
            [settled] => Some(settled),
            _ => unreachable!("a `Promise` takes one argument"),
        },
        IdlTypeInner::Types(_) | IdlTypeInner::Name(_) => None,
    }
}

fn is_undefined(ty: &IdlType) -> bool {
    ty.generic.is_empty() && matches!(&ty.inner, IdlTypeInner::Name(n) if n == "undefined")
}
