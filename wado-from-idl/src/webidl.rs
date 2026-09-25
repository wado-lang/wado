//! WebIDL-to-IR transformation, over the webidl2 AST `scripts/webidl/snapshot.mjs`
//! writes: one unrestricted resource per interface. See `docs/wep-2026-04-01-tide.md`.

use anyhow::{Result, anyhow, bail};
use indexmap::{IndexMap, IndexSet};
use serde::Deserialize;
use wado_compiler::ast::HandleClasses;

use crate::WadoCodeGenerator;
use crate::glue;
use crate::ir::{WadoFunction, WadoInterface, WadoModule, WadoParam, WadoResource, WadoType};
use crate::naming::{to_kebab_case, to_snake_case, to_upper_camel_case, to_wado_identifier};

/// The interface of the functions handing out the first handle.
const GLOBAL_INTERFACE: &str = "global";

/// The file `snapshot.mjs` writes: the slice's definitions, in webidl2's shape.
#[derive(Deserialize)]
pub struct Snapshot {
    /// The `@webref/idl` version the slice was taken from.
    pub webref: String,
    /// The `web:<package>` the slice generates.
    pub package: String,
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
}

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
        #[serde(rename = "idlType")]
        idl_type: IdlType,
        arguments: Vec<Argument>,
        special: String,
    },
    #[serde(rename = "constructor")]
    Constructor {
        arguments: Vec<Argument>,
        #[serde(rename = "extAttrs")]
        ext_attrs: Vec<ExtAttr>,
    },
    /// `const`, `iterable`, `maplike`, `setlike`: nothing a resource carries.
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
pub struct Argument {
    pub name: String,
    #[serde(rename = "idlType")]
    pub idl_type: IdlType,
    pub optional: bool,
    pub variadic: bool,
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
    pub js: IndexMap<String, JsMember>,
    /// Each resource's `WebIDL` interface name, keyed by its Wado name.
    pub interfaces: IndexMap<String, String>,
    /// `Interface.member: reason`, in source order.
    pub skipped: Vec<String>,
}

/// What a function does with the JavaScript object it reaches. `Get`, `Set` and
/// `Call` act on the receiver; the interface names are `WebIDL`'s.
#[derive(Debug)]
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

/// A member's lowering: a function and its JavaScript half, or the Wado name it
/// would have had and why there is none.
type Lowered = std::result::Result<(WadoFunction, JsMember), (String, String)>;

/// A callback's return type and arguments, or why no function stands in for it.
type Signature<'a> = std::result::Result<(&'a IdlType, &'a [Argument]), String>;

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
                            idl_type,
                            arguments,
                            ..
                        } => Some((idl_type, arguments.as_slice())),
                        _ => None,
                    })
                    .collect();
                let signature = match operations[..] {
                    [operation] => Ok(operation),
                    _ => Err(format!(
                        "callback interface of {} operations",
                        operations.len()
                    )),
                };
                (name, signature)
            }
        }
    }
}

/// The module binding the `web:<package>` interfaces and its glue, each naming
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
        skipped: output.skipped,
    })
}

/// A package's generated files, and the members the slice could not express.
pub struct Generated {
    /// The Wado module declaring the `web:<package>` imports.
    pub wado: String,
    /// The JavaScript module serving them from the browser's objects.
    pub glue: String,
    pub skipped: Vec<String>,
}

/// Transform a snapshot into the module binding the `web:<package>` interfaces.
///
/// # Errors
///
/// The slice is not closed (a parent or a mixin it names is missing), a child
/// redeclares an inherited method, or the slice holds two `[Global]` interfaces.
pub fn transform(snapshot: &Snapshot) -> Result<WebIdlOutput> {
    let merged = merge(snapshot)?;
    let lowering = Lowering {
        package: &snapshot.package,
        slice: merged.keys().copied().collect(),
        typedefs: snapshot
            .typedefs
            .iter()
            .map(|t| (t.name.as_str(), &t.idl_type))
            .collect(),
        callbacks: snapshot.callbacks.iter().map(Callback::signature).collect(),
        global_interface: merged
            .values()
            .any(|iface| iface.global)
            .then(|| to_upper_camel_case(&snapshot.package)),
    };

    let classes = number_classes(&merged)?;
    let mut skipped = Vec::new();
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
            },
        );
    }
    reject_overrides(&merged, &resources)?;

    let mut module = WadoModule::new(snapshot.package.clone(), snapshot.webref.clone());
    if let Some(bindings) = lowering.global_bindings(&merged, &resources)? {
        let functions = split_js(bindings, &mut js);
        module.interfaces.push(WadoInterface {
            name: lowering
                .global_interface
                .clone()
                .expect("a package with global bindings has a `[Global]` interface"),
            doc_comment: Some(format!(
                "The `web:{}` entry points, which hand out the first handle.",
                snapshot.package
            )),
            cm_interface: lowering.interface_path(GLOBAL_INTERFACE),
            functions,
        });
    }
    let interfaces = resources
        .iter()
        .map(|(name, r)| (r.name.clone(), (*name).to_string()))
        .collect();
    module.resources = resources.into_values().collect();
    Ok(WebIdlOutput {
        module,
        js,
        interfaces,
        skipped,
    })
}

/// The functions of `bindings`, their JavaScript halves moved into `js`.
fn split_js(
    bindings: Vec<(WadoFunction, JsMember)>,
    js: &mut IndexMap<String, JsMember>,
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

/// A child may not redeclare a method reachable through its chain. Statics
/// (`new` above all) are not inherited.
fn reject_overrides(
    merged: &IndexMap<&str, Merged<'_>>,
    resources: &IndexMap<&str, WadoResource>,
) -> Result<()> {
    let methods: IndexMap<&str, IndexSet<&str>> = resources
        .iter()
        .map(|(name, r)| {
            let receivers = r
                .methods
                .iter()
                .filter(|m| m.params.first().is_some_and(|p| p.name == "self"))
                .map(|m| m.name.as_str())
                .collect();
            (*name, receivers)
        })
        .collect();
    for (name, iface) in merged {
        let mut ancestor = iface.inheritance.as_deref();
        while let Some(parent) = ancestor {
            if let Some(method) = methods[name].intersection(&methods[parent]).next() {
                bail!("`{name}.{method}` redeclares a method inherited from `{parent}`");
            }
            ancestor = merged[parent].inheritance.as_deref();
        }
    }
    Ok(())
}

struct Lowering<'a> {
    package: &'a str,
    slice: IndexSet<&'a str>,
    typedefs: IndexMap<&'a str, &'a IdlType>,
    callbacks: IndexMap<&'a str, Signature<'a>>,
    /// The interface handing out the first handle, which a callback's body may
    /// perform. `None` without a `[Global]` interface.
    global_interface: Option<String>,
}

impl Lowering<'_> {
    fn interface_path(&self, name: &str) -> String {
        format!("web:{}/{}", self.package, to_kebab_case(name))
    }

    /// Every member of `iface` that lowers, appending to `skipped` the ones
    /// that do not and the names two overloads compete for.
    fn methods_of(
        &self,
        iface: &str,
        path: &str,
        merged: &Merged<'_>,
        skipped: &mut Vec<String>,
    ) -> Vec<(WadoFunction, JsMember)> {
        let mut candidates: IndexMap<String, (Vec<(WadoFunction, JsMember)>, Vec<String>)> =
            IndexMap::new();
        for member in &merged.members {
            for lowered in self.lower_member(iface, path, member) {
                match lowered {
                    Ok(binding) => {
                        candidates
                            .entry(binding.0.name.clone())
                            .or_default()
                            .0
                            .push(binding);
                    }
                    Err((name, reason)) => candidates.entry(name).or_default().1.push(reason),
                }
            }
        }
        let mut methods = Vec::new();
        for (name, (mut functions, reasons)) in candidates {
            match functions.len() {
                1 => methods.push(functions.pop().unwrap()),
                0 => skipped.extend(
                    reasons
                        .into_iter()
                        .map(|reason| format!("{iface}.{name}: {reason}")),
                ),
                n => skipped.push(format!("{iface}.{name}: {n} overloads")),
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
                    return vec![Err((getter, "static attribute".to_string()))];
                }
                let kebab = to_kebab_case(name);
                let mut out = vec![match self.lower_type(idl_type, Flow::Out) {
                    Ok(ty) => Ok((
                        function(
                            getter,
                            format!("{path}#{kebab}"),
                            vec![self_param(iface)],
                            Some(ty),
                        ),
                        JsMember::Get(name.clone()),
                    )),
                    Err(reason) => Err((getter, reason)),
                }];
                // `Flow::In` admits everything `Flow::Out` does, so a setter
                // that fails here fails for the reason the getter just gave.
                if let Some(Ok(ty)) = (!readonly).then(|| self.lower_type(idl_type, Flow::In)) {
                    let value = WadoParam {
                        name: "value".to_string(),
                        ty,
                        wit_name: "value".to_string(),
                    };
                    out.push(Ok((
                        function(
                            format!("set_{}", to_snake_case(name)),
                            format!("{path}#set-{kebab}"),
                            vec![self_param(iface), value],
                            None,
                        ),
                        JsMember::Set(name.clone()),
                    )));
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
                    _ => return vec![Err((wado_name, format!("{special} operation")))],
                };
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
                    return vec![Err(("new".to_string(), "HTMLConstructor".to_string()))];
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
            Member::Other => Vec::new(),
        }
    }

    /// `return_type` is `None` for a constructor, which yields `iface`.
    fn lower_operation(
        &self,
        iface: &str,
        (wado_name, js): (String, JsMember),
        cm_attr: String,
        receiver: Option<WadoParam>,
        arguments: &[Argument],
        return_type: Option<&IdlType>,
    ) -> Lowered {
        let skip = |reason: String| Err((wado_name.clone(), reason));
        let return_type = match return_type {
            None => Some(WadoType::Named(to_upper_camel_case(iface))),
            Some(ty) if is_undefined(ty) => None,
            Some(ty) => match self.lower_type(ty, Flow::Out) {
                Ok(ty) => Some(ty),
                Err(reason) => return skip(reason),
            },
        };
        let mut params: Vec<WadoParam> = receiver.into_iter().collect();
        for arg in arguments {
            if arg.variadic {
                return skip(format!("`{}`: variadic", arg.name));
            }
            let ty = match self.lower_type(&arg.idl_type, Flow::In) {
                Ok(ty) => ty,
                // A trailing optional the slice cannot express is left to
                // its WebIDL default; a required one takes the member with it.
                Err(_) if arg.optional => break,
                Err(reason) => return skip(format!("`{}`: {reason}", arg.name)),
            };
            // `None` is the argument left out, so the WebIDL default applies in
            // the browser. A CM operation admits no default argument.
            params.push(WadoParam {
                name: to_wado_identifier(&arg.name),
                ty: optional(ty, arg.optional),
                wit_name: to_kebab_case(&arg.name),
            });
        }
        Ok((function(wado_name, cm_attr, params, return_type), js))
    }

    /// The Wado type of a `WebIDL` type, or why the slice has none. A union is
    /// the one constituent the slice can express; `undefined` in it means nullable.
    fn lower_type(&self, ty: &IdlType, flow: Flow) -> std::result::Result<WadoType, String> {
        if !ty.generic.is_empty() {
            return Err(format!("`{}<…>`", ty.generic));
        }
        let (inner, nullable) = match &ty.inner {
            IdlTypeInner::Name(name) => (self.lower_name(name, flow)?, ty.nullable),
            IdlTypeInner::Types(constituents) => {
                let mut expressible = Vec::new();
                let mut reasons = Vec::new();
                for constituent in constituents.iter().filter(|c| !is_undefined(c)) {
                    match self.lower_type(constituent, flow) {
                        Ok(ty) => expressible.push(ty),
                        Err(reason) => reasons.push(reason),
                    }
                }
                match expressible.len() {
                    1 if reasons.is_empty() || flow == Flow::In => (),
                    1 => {
                        return Err(format!(
                            "union narrowed in a result: {}",
                            reasons.join("; ")
                        ));
                    }
                    0 => return Err(format!("union: {}", reasons.join("; "))),
                    n => return Err(format!("union of {n} expressible types")),
                }
                let nullable = ty.nullable || constituents.iter().any(is_undefined);
                (expressible.pop().unwrap(), nullable)
            }
        };
        Ok(optional(inner, nullable))
    }

    fn lower_name(&self, name: &str, flow: Flow) -> std::result::Result<WadoType, String> {
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
            "undefined" => return Err("`undefined` outside a return type".to_string()),
            _ if self.slice.contains(name) => WadoType::Named(to_upper_camel_case(name)),
            _ => {
                if let Some(target) = self.typedefs.get(name) {
                    return self.lower_type(target, flow);
                }
                return match self.callbacks.get(name) {
                    Some(signature) => self.lower_callback(signature.clone()?, flow),
                    None => Err(format!("`{name}` is outside the slice")),
                };
            }
        })
    }

    /// A callback crosses only into the host, returns nothing, and takes only
    /// what the host can call a closure back with: scalars and handles.
    fn lower_callback(
        &self,
        (return_type, arguments): (&IdlType, &[Argument]),
        flow: Flow,
    ) -> std::result::Result<WadoType, String> {
        if flow == Flow::Out {
            return Err("a callback in a result".to_string());
        }
        if !is_undefined(return_type) {
            return Err("a callback returning a value".to_string());
        }
        let params = arguments
            .iter()
            .map(|arg| {
                let ty = self.lower_type(&arg.idl_type, Flow::Out)?;
                match ty {
                    _ if arg.variadic => Err(format!("callback argument `{}`: variadic", arg.name)),
                    WadoType::Named(_)
                    | WadoType::Bool
                    | WadoType::I8
                    | WadoType::I16
                    | WadoType::I32
                    | WadoType::I64
                    | WadoType::U8
                    | WadoType::U16
                    | WadoType::U32
                    | WadoType::U64
                    | WadoType::F32
                    | WadoType::F64 => Ok(ty),
                    _ => Err(format!(
                        "callback argument `{}`: neither a scalar nor a handle",
                        arg.name
                    )),
                }
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
    ) -> Result<Option<Vec<(WadoFunction, JsMember)>>> {
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
            JsMember::Global,
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
                        JsMember::GlobalGet(name.clone()),
                    ));
                }
            }
        }
        Ok(Some(bindings))
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
    }
}

/// `ty` as an `Option` when `wrap`, without doubling one it already is. A
/// callback stays required: the DOM ignores a null listener, so leaving the call
/// out says the same.
fn optional(ty: WadoType, wrap: bool) -> WadoType {
    match ty {
        WadoType::Option(_) | WadoType::Callback { .. } => ty,
        ty if wrap => WadoType::Option(Box::new(ty)),
        ty => ty,
    }
}

fn is_undefined(ty: &IdlType) -> bool {
    ty.generic.is_empty() && matches!(&ty.inner, IdlTypeInner::Name(n) if n == "undefined")
}
