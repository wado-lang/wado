//! WebIDL-to-IR transformation, over the webidl2 AST `scripts/webidl/snapshot.mjs`
//! writes: one extern-handle resource per interface. See `docs/wep-2026-04-01-tide.md`.

use anyhow::{Result, bail};
use indexmap::{IndexMap, IndexSet};
use serde::Deserialize;

use crate::ir::{WadoFunction, WadoInterface, WadoModule, WadoParam, WadoResource, WadoType};
use crate::naming::{to_kebab_case, to_snake_case, to_upper_camel_case, to_wado_identifier};

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

/// The generated module, and every member the slice could not express.
#[derive(Debug)]
pub struct WebIdlOutput {
    pub module: WadoModule,
    /// `Interface.member: reason`, in source order.
    pub skipped: Vec<String>,
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

/// A member's lowering: a function, or the Wado name it would have had and
/// why there is none.
type Lowered = std::result::Result<WadoFunction, (String, String)>;

/// The `web:<package>` module's source, naming `source` in its header, and
/// the skipped members.
///
/// # Errors
///
/// See [`transform`].
pub fn generate(snapshot: &Snapshot, source: &str) -> Result<(String, Vec<String>)> {
    let WebIdlOutput {
        mut module,
        skipped,
    } = transform(snapshot)?;
    module.source_files = vec![source.to_string()];
    module.stdlib_identity = Some(format!("web:{}", snapshot.package));
    Ok((crate::WadoCodeGenerator::new().generate(&module), skipped))
}

/// Transform a snapshot into the `web:<package>` module.
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
    };

    let mut skipped = Vec::new();
    let mut resources: IndexMap<&str, WadoResource> = IndexMap::new();
    for (name, iface) in &merged {
        let path = lowering.interface_path(name);
        let methods = lowering.methods_of(name, &path, iface, &mut skipped);
        resources.insert(
            name,
            WadoResource {
                name: to_upper_camel_case(name),
                doc_comment: None,
                cm_attr: path,
                extern_handle: true,
                extends: iface.inheritance.as_deref().map(to_upper_camel_case),
                methods,
            },
        );
    }
    reject_overrides(&merged, &resources)?;

    let mut module = WadoModule::new(snapshot.package.clone(), snapshot.webref.clone());
    module
        .interfaces
        .extend(lowering.global_effect(&merged, &resources)?);
    module.resources = resources.into_values().collect();
    Ok(WebIdlOutput { module, skipped })
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
    ) -> Vec<WadoFunction> {
        let mut candidates: IndexMap<String, (Vec<WadoFunction>, Vec<String>)> = IndexMap::new();
        for member in &merged.members {
            for lowered in self.lower_member(iface, path, member) {
                match lowered {
                    Ok(function) => {
                        candidates
                            .entry(function.name.clone())
                            .or_default()
                            .0
                            .push(function);
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
                let ty = match self.lower_type(idl_type) {
                    Ok(ty) => ty,
                    Err(reason) => return vec![Err((getter, reason))],
                };
                let kebab = to_kebab_case(name);
                let mut out = vec![Ok(function(
                    getter,
                    format!("{path}#{kebab}"),
                    vec![self_param(iface)],
                    Some(ty.clone()),
                ))];
                if !readonly {
                    let value = WadoParam {
                        name: "value".to_string(),
                        ty,
                        wit_name: "value".to_string(),
                    };
                    out.push(Ok(function(
                        format!("set_{}", to_snake_case(name)),
                        format!("{path}#set-{kebab}"),
                        vec![self_param(iface), value],
                        None,
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
                let receiver = match special.as_str() {
                    "" => Some(self_param(iface)),
                    "static" => None,
                    _ => return vec![Err((wado_name, format!("{special} operation")))],
                };
                vec![self.lower_operation(
                    iface,
                    wado_name,
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
                vec![self.lower_operation(
                    iface,
                    "new".to_string(),
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
        wado_name: String,
        cm_attr: String,
        receiver: Option<WadoParam>,
        arguments: &[Argument],
        return_type: Option<&IdlType>,
    ) -> Lowered {
        let skip = |reason: String| Err((wado_name.clone(), reason));
        let return_type = match return_type {
            None => Some(WadoType::Named(to_upper_camel_case(iface))),
            Some(ty) if is_undefined(ty) => None,
            Some(ty) => match self.lower_type(ty) {
                Ok(ty) => Some(ty),
                Err(reason) => return skip(reason),
            },
        };
        let mut params: Vec<WadoParam> = receiver.into_iter().collect();
        for arg in arguments {
            if arg.variadic {
                return skip(format!("`{}`: variadic", arg.name));
            }
            let ty = match self.lower_type(&arg.idl_type) {
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
        Ok(function(wado_name, cm_attr, params, return_type))
    }

    /// The Wado type of a `WebIDL` type, or why the slice has none. A union is
    /// the one constituent the slice can express; `undefined` in it means nullable.
    fn lower_type(&self, ty: &IdlType) -> std::result::Result<WadoType, String> {
        if !ty.generic.is_empty() {
            return Err(format!("`{}<…>`", ty.generic));
        }
        let (inner, nullable) = match &ty.inner {
            IdlTypeInner::Name(name) => (self.lower_name(name)?, ty.nullable),
            IdlTypeInner::Types(constituents) => {
                let mut expressible = Vec::new();
                let mut reasons = Vec::new();
                for constituent in constituents.iter().filter(|c| !is_undefined(c)) {
                    match self.lower_type(constituent) {
                        Ok(ty) => expressible.push(ty),
                        Err(reason) => reasons.push(reason),
                    }
                }
                match expressible.len() {
                    1 => (),
                    0 => return Err(format!("union: {}", reasons.join("; "))),
                    n => return Err(format!("union of {n} expressible types")),
                }
                let nullable = ty.nullable || constituents.iter().any(is_undefined);
                (expressible.pop().unwrap(), nullable)
            }
        };
        Ok(optional(inner, nullable))
    }

    fn lower_name(&self, name: &str) -> std::result::Result<WadoType, String> {
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
            _ => match self.typedefs.get(name) {
                Some(target) => return self.lower_type(target),
                None => return Err(format!("`{name}` is outside the slice")),
            },
        })
    }

    /// The effect handing out the first handle: the `[Global]` interface, and
    /// each of its read-only attributes typed as another slice resource.
    fn global_effect(
        &self,
        merged: &IndexMap<&str, Merged<'_>>,
        resources: &IndexMap<&str, WadoResource>,
    ) -> Result<Option<WadoInterface>> {
        let mut globals = merged.iter().filter(|(_, iface)| iface.global);
        let Some((name, global)) = globals.next() else {
            return Ok(None);
        };
        if let Some((second, _)) = globals.next() {
            bail!("a package has one `[Global]` interface; the slice has `{name}` and `{second}`");
        }
        let path = self.interface_path("global");
        let accessor = |wado_name: String, kebab: &str, ty: &str| {
            function(
                wado_name,
                format!("{path}#{kebab}"),
                Vec::new(),
                Some(WadoType::Named(ty.to_string())),
            )
        };
        let global_type = to_upper_camel_case(name);
        let mut functions = vec![accessor(
            to_wado_identifier(name),
            &to_kebab_case(name),
            &global_type,
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
                && let Ok(WadoType::Named(ty)) = self.lower_type(idl_type)
                && ty != global_type
            {
                let wado_name = to_wado_identifier(name);
                if methods.contains(wado_name.as_str()) {
                    functions.push(accessor(wado_name, &to_kebab_case(name), &ty));
                }
            }
        }
        Ok(Some(WadoInterface {
            name: to_upper_camel_case(self.package),
            doc_comment: Some(format!(
                "The `web:{}` entry points, which hand out the first handle.",
                self.package
            )),
            cm_interface: path,
            functions,
        }))
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

/// `ty` as an `Option` when `wrap`, without doubling one it already is.
fn optional(ty: WadoType, wrap: bool) -> WadoType {
    match ty {
        WadoType::Option(_) => ty,
        ty if wrap => WadoType::Option(Box::new(ty)),
        ty => ty,
    }
}

fn is_undefined(ty: &IdlType) -> bool {
    ty.generic.is_empty() && matches!(&ty.inner, IdlTypeInner::Name(n) if n == "undefined")
}
