//! WIT text emission from [`Semantics`].
//!
//! This is the producer side of WIT interoperability (WEP
//! `wep-2026-05-02-wit-interoperability.md`, Phase 1). It takes the frontend
//! output — declared interfaces, exported items, the active world, and the
//! type table — and renders a WIT document with [`wit_encoder`].
//!
//! `wado wit` prints the text; `wado compile` (Phase 2) reuses the same text
//! to derive the embedded `component-type` custom section. WIT is fully
//! determined by name and type resolution, so emission reads [`Semantics`]
//! and never touches monomorphize / lower / codegen.

use std::collections::{BTreeMap, BTreeSet};

use wit_encoder::{
    Field, Flag, Interface, Package, PackageName, Params, StandaloneFunc, Type, TypeDef,
    VariantCase, World, WorldItem,
};

use crate::semantics::Semantics;
use crate::tir::{PrimitiveType, ResolvedType, TypeId, TypeTable};

/// How much of the referenced interface graph to inline into the WIT document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WitScope {
    /// Inline every referenced interface, including stdlib WASI/CM, producing
    /// a self-describing document.
    #[default]
    Full,
    /// Inline only user-authored interfaces; stdlib references stay as bare
    /// imports resolved against an external registry.
    Local,
}

/// Options threaded from the CLI into the emitter. These are project-level
/// configuration, not frontend-derived facts, so they live here rather than on
/// [`Semantics`].
#[derive(Debug, Clone)]
pub struct WitEmitOptions {
    /// Inlining scope for referenced interfaces.
    pub scope: WitScope,
    /// Fully-qualified name of the target world (e.g. `wasi:cli/command`).
    pub world_fq: String,
    /// Name for the synthesized default interface that groups bare exports.
    /// Sourced from `[package].name` or the entry-file stem.
    pub default_interface_name: String,
}

/// A failure that prevents emitting valid WIT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WitEmitError {
    /// Analysis did not complete, so the frontend facts are unavailable.
    IncompleteSemantics,
    /// A type appears in an exported signature that has no WIT representation.
    UnrepresentableType {
        /// Human-readable description of the offending type.
        description: String,
    },
}

impl std::fmt::Display for WitEmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IncompleteSemantics => {
                write!(f, "cannot emit WIT: semantic analysis did not complete")
            }
            Self::UnrepresentableType { description } => {
                write!(f, "type is not representable in WIT: {description}")
            }
        }
    }
}

impl std::error::Error for WitEmitError {}

/// Render the WIT text for `sem` under `opts`.
pub fn emit_wit_text(sem: &Semantics, opts: &WitEmitOptions) -> Result<String, WitEmitError> {
    if !sem.is_complete() {
        return Err(WitEmitError::IncompleteSemantics);
    }

    let mut emitter = Emitter::new(sem);
    let package = emitter.build_package(opts)?;
    Ok(package.to_string())
}

/// Drives one emission pass over the frontend's TIR modules.
struct Emitter<'a> {
    sem: &'a Semantics,
    types: &'a TypeTable,
    /// User-authored type declarations keyed by source name, gathered across
    /// every loaded user module so referenced types can be looked up by name.
    decls: TypeDecls<'a>,
    /// Named user types referenced by emitted signatures, in discovery order,
    /// awaiting a `TypeDef`. Keyed by source name to dedupe.
    pending: BTreeMap<String, TypeId>,
    /// Source names already emitted as a `TypeDef`.
    emitted: BTreeSet<String>,
}

/// Name-indexed view of the user-authored type declarations.
#[derive(Default)]
struct TypeDecls<'a> {
    structs: BTreeMap<String, &'a crate::tir::TirStruct>,
    enums: BTreeMap<String, &'a crate::tir::TirEnum>,
    variants: BTreeMap<String, &'a crate::tir::TirVariantDecl>,
    flags: BTreeMap<String, &'a crate::tir::TirFlags>,
    newtypes: BTreeMap<String, &'a crate::tir::TirNewtype>,
}

impl<'a> Emitter<'a> {
    fn new(sem: &'a Semantics) -> Self {
        let mut decls = TypeDecls::default();
        for module in sem.tir_modules.values() {
            for s in &module.structs {
                decls.structs.insert(s.name.clone(), s);
            }
            for e in &module.enums {
                decls.enums.insert(e.name.clone(), e);
            }
            for v in &module.variants {
                decls.variants.insert(v.name.clone(), v);
            }
            for fl in &module.flags {
                decls.flags.insert(fl.name.clone(), fl);
            }
            for nt in &module.newtypes {
                decls.newtypes.insert(nt.name.clone(), nt);
            }
        }
        Self {
            sem,
            types: &sem.types,
            decls,
            pending: BTreeMap::new(),
            emitted: BTreeSet::new(),
        }
    }

    fn build_package(&mut self, opts: &WitEmitOptions) -> Result<Package, WitEmitError> {
        let exports = self.collect_exported_functions();

        // Render every exported function signature first, which seeds `pending`
        // with the user types they reference.
        let mut funcs: Vec<StandaloneFunc> = Vec::new();
        for (name, params, ret) in &exports {
            let mut func = StandaloneFunc::new(to_kebab(name), false);
            let mut wit_params = Params::empty();
            for (pname, pty) in params {
                wit_params.push(to_kebab(pname), self.map_type(*pty)?);
            }
            func.set_params(wit_params);
            func.set_result(self.map_return(*ret)?);
            funcs.push(func);
        }

        // Expand the transitive closure of referenced user types into TypeDefs.
        let type_defs = self.drain_pending_type_defs()?;

        let mut package = Package::new(PackageName::new("root", "component", None));
        let mut world = World::new(to_kebab(&world_local_name(&opts.world_fq)));

        if funcs.is_empty() {
            // No exports: an empty world (see "World-less libraries" in the WEP).
        } else if type_defs.is_empty() {
            // Only functions, no referenced user types: direct world exports.
            for func in funcs {
                world.item(WorldItem::function_export(func));
            }
        } else {
            // Group exports and their types into the default interface.
            let iface_name = to_kebab(&opts.default_interface_name);
            let mut iface = Interface::new(iface_name.clone());
            for ty in type_defs {
                iface.type_def(ty);
            }
            for func in funcs {
                iface.function(func);
            }
            package.interface(iface);
            world.named_interface_export(iface_name);
        }

        package.world(world);
        Ok(package)
    }

    /// Exported functions across every loaded user module, in module-then-decl
    /// order: `(name, params, return_type)`.
    fn collect_exported_functions(&self) -> Vec<(String, Vec<(String, TypeId)>, TypeId)> {
        let mut out = Vec::new();
        for module in self.sem.tir_modules.values() {
            // Only user-authored modules contribute to the WIT contract; the
            // bundled allocator / runtime modules also carry `export fn`
            // (canonical-ABI realloc), but those are not part of the contract.
            let source = &module.module_source;
            if !(source.is_entry_point() || source.is_local()) {
                continue;
            }
            for func_rc in &module.functions {
                let func = func_rc.borrow();
                if !func.is_export {
                    continue;
                }
                let params = func
                    .params
                    .iter()
                    .map(|p| (p.name.clone(), p.type_id))
                    .collect();
                out.push((func.name.clone(), params, func.return_type));
            }
        }
        out
    }

    fn drain_pending_type_defs(&mut self) -> Result<Vec<TypeDef>, WitEmitError> {
        let mut out = Vec::new();
        while let Some((name, type_id)) = self.next_pending() {
            if self.emitted.contains(&name) {
                continue;
            }
            self.emitted.insert(name.clone());
            if let Some(def) = self.emit_type_def(&name, type_id)? {
                out.push(def);
            }
        }
        Ok(out)
    }

    fn next_pending(&mut self) -> Option<(String, TypeId)> {
        let key = self.pending.keys().next().cloned()?;
        let id = self.pending.remove(&key)?;
        Some((key, id))
    }

    fn emit_type_def(
        &mut self,
        name: &str,
        _type_id: TypeId,
    ) -> Result<Option<TypeDef>, WitEmitError> {
        let kebab = to_kebab(name);
        if let Some(s) = self.decls.structs.get(name).copied() {
            let mut fields = Vec::new();
            for field in &s.fields {
                fields.push(Field::new(
                    to_kebab(&field.name),
                    self.map_type(field.type_id)?,
                ));
            }
            return Ok(Some(TypeDef::record(kebab, fields)));
        }
        if let Some(e) = self.decls.enums.get(name).copied() {
            let cases = e.cases.iter().map(|c| to_kebab(&c.name));
            return Ok(Some(TypeDef::enum_(kebab, cases)));
        }
        if let Some(v) = self.decls.variants.get(name).copied() {
            let mut cases = Vec::new();
            for case in &v.cases {
                let payload = self.map_variant_payload(case.payload)?;
                cases.push(match payload {
                    Some(ty) => VariantCase::value(to_kebab(&case.name), ty),
                    None => VariantCase::empty(to_kebab(&case.name)),
                });
            }
            return Ok(Some(TypeDef::variant(kebab, cases)));
        }
        if let Some(fl) = self.decls.flags.get(name).copied() {
            let members = fl.members.iter().map(|m| Flag::new(to_kebab(&m.name)));
            return Ok(Some(TypeDef::flags(kebab, members)));
        }
        if let Some(nt) = self.decls.newtypes.get(name).copied() {
            let base = self.map_type(nt.type_id)?;
            return Ok(Some(TypeDef::type_(kebab, base)));
        }
        Err(WitEmitError::UnrepresentableType {
            description: format!("`{name}` has no emittable declaration"),
        })
    }

    /// Map a return-position type: `()` becomes no result.
    fn map_return(&mut self, type_id: TypeId) -> Result<Option<Type>, WitEmitError> {
        if matches!(self.types.get(type_id), ResolvedType::Unit) {
            return Ok(None);
        }
        Ok(Some(self.map_type(type_id)?))
    }

    /// Map a variant case payload: unit payload becomes no payload.
    fn map_variant_payload(&mut self, type_id: TypeId) -> Result<Option<Type>, WitEmitError> {
        if matches!(self.types.get(type_id), ResolvedType::Unit) {
            return Ok(None);
        }
        Ok(Some(self.map_type(type_id)?))
    }

    /// Map a value-position Wado type to its WIT counterpart, recording any
    /// referenced user types for later `TypeDef` emission.
    fn map_type(&mut self, type_id: TypeId) -> Result<Type, WitEmitError> {
        match self.types.get(type_id) {
            ResolvedType::Primitive(p) => map_primitive(*p),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => self.map_type(*inner),
            ResolvedType::BuiltinArray(inner) => Ok(Type::list(self.map_type(*inner)?)),
            ResolvedType::Struct { name, .. } if name == "String" => Ok(Type::String),
            ResolvedType::Struct { name, .. } => Ok(self.named(name, type_id)),
            ResolvedType::Enum { name, .. }
            | ResolvedType::Variant { name, .. }
            | ResolvedType::Flags { name, .. }
            | ResolvedType::Newtype { name, .. } => Ok(self.named(name, type_id)),
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => self.map_generic(name, type_args.clone()),
            other => Err(WitEmitError::UnrepresentableType {
                description: describe_type(other),
            }),
        }
    }

    fn map_generic(&mut self, name: &str, args: Vec<TypeId>) -> Result<Type, WitEmitError> {
        match name {
            "Option" if args.len() == 1 => Ok(Type::option(self.map_type(args[0])?)),
            "List" if args.len() == 1 => Ok(Type::list(self.map_type(args[0])?)),
            "Result" if args.len() == 2 => {
                let ok = self.map_type(args[0])?;
                let err = self.map_type(args[1])?;
                Ok(Type::result_both(ok, err))
            }
            "Tuple" => {
                let mut elems = Vec::new();
                for a in args {
                    elems.push(self.map_type(a)?);
                }
                Ok(Type::tuple(elems))
            }
            _ => Err(WitEmitError::UnrepresentableType {
                description: format!("generic `{name}` with {} type argument(s)", args.len()),
            }),
        }
    }

    /// Reference a named user type, queuing it for `TypeDef` emission.
    fn named(&mut self, name: &str, type_id: TypeId) -> Type {
        if !self.emitted.contains(name) {
            self.pending.entry(name.to_string()).or_insert(type_id);
        }
        Type::named(to_kebab(name))
    }
}

fn map_primitive(p: PrimitiveType) -> Result<Type, WitEmitError> {
    let ty = match p {
        PrimitiveType::I8 => Type::S8,
        PrimitiveType::I16 => Type::S16,
        PrimitiveType::I32 => Type::S32,
        PrimitiveType::I64 => Type::S64,
        PrimitiveType::U8 => Type::U8,
        PrimitiveType::U16 => Type::U16,
        PrimitiveType::U32 => Type::U32,
        PrimitiveType::U64 => Type::U64,
        PrimitiveType::F32 => Type::F32,
        PrimitiveType::F64 => Type::F64,
        PrimitiveType::Bool => Type::Bool,
        PrimitiveType::Char => Type::Char,
        PrimitiveType::I128 | PrimitiveType::U128 | PrimitiveType::V128 => {
            return Err(WitEmitError::UnrepresentableType {
                description: format!("`{}` has no WIT representation", p.as_str()),
            });
        }
    };
    Ok(ty)
}

fn describe_type(ty: &ResolvedType) -> String {
    match ty {
        ResolvedType::Function { .. } => "function type".to_string(),
        ResolvedType::TypeParam { name, .. } => format!("type parameter `{name}`"),
        ResolvedType::Unit => "unit `()`".to_string(),
        ResolvedType::Never => "never `!`".to_string(),
        other => format!("{other:?}"),
    }
}

/// Extract the local name of a world FQ: `wasi:cli/command` -> `command`.
fn world_local_name(world_fq: &str) -> String {
    world_fq
        .rsplit('/')
        .next()
        .unwrap_or(world_fq)
        .split('@')
        .next()
        .unwrap_or(world_fq)
        .to_string()
}

/// Convert a Wado identifier (`snake_case` / `PascalCase` / `camelCase`) to WIT
/// kebab-case.
fn to_kebab(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_lower_or_digit = false;
    for ch in name.chars() {
        if ch == '_' {
            if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
            prev_lower_or_digit = false;
            continue;
        }
        if ch.is_ascii_uppercase() {
            if prev_lower_or_digit {
                out.push('-');
            }
            out.push(ch.to_ascii_lowercase());
            prev_lower_or_digit = false;
        } else {
            out.push(ch);
            prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kebab_cases() {
        assert_eq!(to_kebab("distance"), "distance");
        assert_eq!(to_kebab("MyApi"), "my-api");
        assert_eq!(to_kebab("set_level"), "set-level");
        assert_eq!(to_kebab("HTTPServer"), "http-server");
        assert_eq!(to_kebab("parse2html"), "parse2html");
    }

    #[test]
    fn world_local_names() {
        assert_eq!(world_local_name("wasi:cli/command"), "command");
        assert_eq!(world_local_name("wasi:http/service@0.3.0"), "service");
        assert_eq!(world_local_name("root"), "root");
    }
}
