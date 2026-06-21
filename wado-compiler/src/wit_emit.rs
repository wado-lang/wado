//! WIT text emission from [`Semantics`].
//!
//! This is the producer side of WIT interoperability (WEP
//! `wep-2026-05-02-wit-interoperability.md`, Phase 1). It takes the frontend
//! output — declared interfaces, exported items, the active world, and the
//! type table — and renders a WIT document with [`wit_encoder`].
//!
//! `wado wit` prints the text. A planned Phase 2 (`wado compile` embedding, not
//! yet implemented) will reuse the same text to derive the embedded
//! `component-type` custom section. WIT is fully determined by name and type
//! resolution, so emission reads [`Semantics`] and never touches monomorphize /
//! lower / codegen.

use std::collections::{BTreeMap, BTreeSet};

use wit_encoder::{
    Field, Flag, Interface, Package, PackageName, Params, ResourceFunc, StandaloneFunc, Type,
    TypeDef, VariantCase, World, WorldItem,
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
    /// The CM interface FQs the compiled component imports — the WIR-level
    /// import plan (`NirPackage::imported_cm_interfaces`). The authoritative,
    /// faithful world import set; injected by the caller because it is computed
    /// post-DCE and is unavailable from `Semantics` alone.
    pub world_imports: Vec<String>,
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
    let mut out = package.to_string();

    // `full` scope inlines every referenced CM interface as a nested package,
    // preserving the original package/version structure (the same shape
    // `wasm-tools component wit` emits), so the document is self-describing and
    // re-parses without an external registry.
    if opts.scope == WitScope::Full {
        for nested in emitter.build_nested_packages()? {
            out.push('\n');
            out.push_str(&nested.to_string());
        }
    }
    Ok(out)
}

/// Drives one emission pass over the frontend's TIR modules.
struct Emitter<'a> {
    sem: &'a Semantics,
    types: &'a TypeTable,
    /// User-authored type declarations keyed by source name, gathered across
    /// every loaded user module so referenced types can be looked up by name.
    decls: TypeDecls<'a>,
    /// CM interface FQs referenced by the world (imports + exports), to inline
    /// under `full` scope. Populated by `build_package`.
    referenced_interfaces: BTreeSet<String>,
    /// Named user types referenced by emitted signatures, in discovery order,
    /// awaiting a `TypeDef`. Keyed by source name to dedupe.
    pending: BTreeMap<String, TypeId>,
    /// Source names already emitted as a `TypeDef`.
    emitted: BTreeSet<String>,
}

/// One exported function gathered from the frontend, before WIT rendering.
struct ExportedFn {
    name: String,
    params: Vec<(String, TypeId)>,
    return_type: TypeId,
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
            referenced_interfaces: BTreeSet::new(),
            pending: BTreeMap::new(),
            emitted: BTreeSet::new(),
        }
    }

    fn build_package(&mut self, opts: &WitEmitOptions) -> Result<Package, WitEmitError> {
        let exports = self.collect_exported_functions();
        let world_info = self
            .sem
            .world_registry()
            .and_then(|registry| registry.get(&opts.world_fq));

        // World imports: the faithful set the compiled component imports,
        // computed post-DCE at the WIR layer and injected by the caller (WEP
        // `wep-2026-05-02-wit-interoperability.md` §"Faithful imports"). This
        // includes implicit runtime imports (e.g. `wasi:cli/stderr` for assert)
        // and excludes type-alias-only interfaces — neither visible from the
        // effect rows alone.
        let import_fqs: BTreeSet<String> = opts.world_imports.iter().cloned().collect();

        // Partition exports into world-conformance entry points (`run` /
        // `handle`, which map to a standard export interface like
        // `wasi:cli/run`) and ordinary user exports.
        let mut export_fqs: BTreeSet<String> = BTreeSet::new();
        let mut user_funcs: Vec<StandaloneFunc> = Vec::new();
        for export in &exports {
            if let Some(fq) = world_info.and_then(|info| {
                info.exports
                    .iter()
                    .find(|e| e.name == export.name)
                    .and_then(|e| e.from_interface_fq.clone())
            }) {
                export_fqs.insert(fq);
                continue;
            }
            user_funcs.push(self.render_function(export)?);
        }

        // `full`-scope nested-package bodies need the full type closure over the
        // imports and exports — a superset of the world import list, because a
        // `use duration` reference pulls in `wasi:clocks/types` even though the
        // component does not import that type-alias-only interface.
        let mut referenced: BTreeSet<String> = self.transitive_import_closure(import_fqs.clone());
        referenced.extend(self.transitive_import_closure(export_fqs.clone()));
        self.referenced_interfaces = referenced;

        // Expand the transitive closure of referenced user types into TypeDefs.
        let type_defs = self.drain_pending_type_defs()?;

        let mut package = Package::new(PackageName::new("root", "component", None));
        let mut world = World::new(to_kebab(&world_local_name(&opts.world_fq)));

        for fq in &import_fqs {
            world.named_interface_import(fq.clone());
        }
        for fq in &export_fqs {
            world.named_interface_export(fq.clone());
        }

        if user_funcs.is_empty() {
            // No ordinary user exports beyond world conformance.
        } else if type_defs.is_empty() {
            // Only functions, no referenced user types: direct world exports.
            for func in user_funcs {
                world.item(WorldItem::function_export(func));
            }
        } else {
            // Group user exports and their types into the default interface.
            let iface_name = to_kebab(&opts.default_interface_name);
            let mut iface = Interface::new(iface_name.clone());
            for ty in type_defs {
                iface.type_def(ty);
            }
            for func in user_funcs {
                iface.function(func);
            }
            package.interface(iface);
            world.named_interface_export(iface_name);
        }

        package.world(world);
        Ok(package)
    }

    /// Render one exported function to a WIT `StandaloneFunc`, seeding `pending`
    /// with any user types it references.
    fn render_function(&mut self, export: &ExportedFn) -> Result<StandaloneFunc, WitEmitError> {
        let mut func = StandaloneFunc::new(to_kebab(&export.name), false);
        let mut wit_params = Params::empty();
        for (pname, pty) in &export.params {
            wit_params.push(to_kebab(pname), self.map_type(*pty)?);
        }
        func.set_params(wit_params);
        func.set_result(self.map_return(export.return_type)?);
        Ok(func)
    }

    /// Exported functions across every loaded user module, in module-then-decl
    /// order.
    fn collect_exported_functions(&self) -> Vec<ExportedFn> {
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
                out.push(ExportedFn {
                    name: func.name.clone(),
                    params,
                    return_type: func.return_type,
                });
            }
        }
        out
    }

    /// Expand a set of directly-used CM interface FQs to the full set the
    /// component imports, following each interface's function and type
    /// signatures to the interfaces they reference.
    fn transitive_import_closure(&self, roots: BTreeSet<String>) -> BTreeSet<String> {
        let Some(registry) = self.sem.cm_interface_registry() else {
            return roots;
        };
        let infos: Vec<crate::component_model::CmInterfaceInfo> = registry.interfaces().collect();

        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut work: Vec<String> = roots.into_iter().collect();
        while let Some(fq) = work.pop() {
            if !visited.insert(fq.clone()) {
                continue;
            }
            // Interfaces referenced by this interface's function signatures.
            for info in infos.iter().filter(|i| i.path == fq) {
                for func in &info.functions {
                    for (_, _, ty) in &func.params {
                        collect_named_type_sources(ty, &mut work);
                    }
                    if let Some(ret) = &func.return_type {
                        collect_named_type_sources(ret, &mut work);
                    }
                }
            }
            // Interfaces referenced by the types this interface defines.
            for (_, _, fields) in registry.structs_for_interface(&fq) {
                for (_, ty) in fields {
                    collect_named_type_sources(ty, &mut work);
                }
            }
            for (_, _, cases) in registry.variants_for_interface(&fq) {
                for case in cases {
                    if let Some(payload) = &case.payload {
                        collect_named_type_sources(payload, &mut work);
                    }
                }
            }
        }
        visited
    }

    /// Build one nested `package` per referenced CM package, each holding the
    /// full reconstructed definitions of the referenced interfaces in it.
    fn build_nested_packages(&self) -> Result<Vec<wit_encoder::NestedPackage>, WitEmitError> {
        let Some(registry) = self.sem.cm_interface_registry() else {
            return Ok(Vec::new());
        };
        let infos: Vec<crate::component_model::CmInterfaceInfo> = registry.interfaces().collect();

        // Group the referenced interface FQs by their owning package.
        let mut by_package: BTreeMap<(String, String, String), Vec<String>> = BTreeMap::new();
        for fq in &self.referenced_interfaces {
            let Some(parts) = FqParts::parse(fq) else {
                continue;
            };
            by_package
                .entry((parts.namespace, parts.package, parts.version))
                .or_default()
                .push(fq.clone());
        }

        let mut packages = Vec::new();
        for ((namespace, package, version), fqs) in by_package {
            let semver = semver::Version::parse(&version).map_err(|_| {
                WitEmitError::UnrepresentableType {
                    description: format!("package version `{version}` is not valid semver"),
                }
            })?;
            let name = PackageName::new(namespace, package, Some(semver));
            let mut nested = wit_encoder::NestedPackage::new(name);
            for fq in fqs {
                nested.interface(self.reconstruct_interface(&fq, &infos, registry)?);
            }
            packages.push(nested);
        }
        Ok(packages)
    }

    /// Reconstruct one WIT `interface` from the CM registry: its functions, the
    /// types it defines, and `use` statements for types it borrows from other
    /// interfaces.
    fn reconstruct_interface(
        &self,
        fq: &str,
        infos: &[crate::component_model::CmInterfaceInfo],
        registry: &crate::component_model::CmInterfaceRegistry,
    ) -> Result<Interface, WitEmitError> {
        let local_name = FqParts::parse(fq)
            .map(|p| p.interface)
            .unwrap_or_else(|| fq.to_string());
        let mut iface = Interface::new(local_name);
        let mut uses: Vec<(String, String)> = Vec::new();

        // Types the interface defines.
        for (_, cm_name, fields) in registry.structs_for_interface(fq) {
            let mut wit_fields = Vec::new();
            for (field_name, field_ty) in fields {
                wit_fields.push(Field::new(
                    field_name.clone(),
                    self.map_ast_type(field_ty, fq, &mut uses)?,
                ));
            }
            iface.type_def(TypeDef::record(cm_name.to_string(), wit_fields));
        }
        for (_, cm_name, cases) in registry.variants_for_interface(fq) {
            let mut wit_cases = Vec::new();
            for case in cases {
                match &case.payload {
                    Some(payload) => wit_cases.push(VariantCase::value(
                        case.cm_name.clone(),
                        self.map_ast_type(payload, fq, &mut uses)?,
                    )),
                    None => wit_cases.push(VariantCase::empty(case.cm_name.clone())),
                }
            }
            iface.type_def(TypeDef::variant(cm_name.to_string(), wit_cases));
        }
        for (_, cm_name, variants) in registry.enums_for_interface(fq) {
            iface.type_def(TypeDef::enum_(
                cm_name.to_string(),
                variants.iter().map(String::clone),
            ));
        }
        for (_, cm_name, members) in registry.flags_for_interface(fq) {
            iface.type_def(TypeDef::flags(
                cm_name.to_string(),
                members.iter().map(|m| Flag::new(m.clone())),
            ));
        }
        for (wado_name, base) in registry.newtypes_for_interface(fq) {
            iface.type_def(TypeDef::type_(
                to_kebab(wado_name),
                self.map_ast_type(base, fq, &mut uses)?,
            ));
        }

        // Functions: resource methods/statics/constructors nest under their
        // resource; everything else is a free interface function.
        let mut resource_funcs: BTreeMap<String, Vec<ResourceFunc>> = BTreeMap::new();
        let mut free_funcs: Vec<StandaloneFunc> = Vec::new();
        for info in infos.iter().filter(|i| i.path == fq) {
            for func in &info.functions {
                if let Some((kind, resource, member)) = parse_resource_func(&func.wasi_func_name) {
                    let rf = self.build_resource_func(func, kind, member, fq, &mut uses)?;
                    resource_funcs
                        .entry(resource.to_string())
                        .or_default()
                        .push(rf);
                } else {
                    let mut wit_func =
                        StandaloneFunc::new(func.wasi_func_name.clone(), func.is_async);
                    let mut params = Params::empty();
                    for (_, cm_name, ty) in &func.params {
                        params.push(cm_name.clone(), self.map_ast_type(ty, fq, &mut uses)?);
                    }
                    wit_func.set_params(params);
                    wit_func.set_result(self.map_cm_result(
                        &func.return_type,
                        func.is_async,
                        fq,
                        &mut uses,
                    )?);
                    free_funcs.push(wit_func);
                }
            }
        }

        // Resources, in declaration order, each with its reconstructed methods.
        let mut resource_order: Vec<String> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for (_, cm_name) in registry.resources_for_interface(fq) {
            if seen.insert(cm_name.to_string()) {
                resource_order.push(cm_name.to_string());
            }
        }
        for key in resource_funcs.keys() {
            if seen.insert(key.clone()) {
                resource_order.push(key.clone());
            }
        }
        for resource in resource_order {
            let funcs = resource_funcs.remove(&resource).unwrap_or_default();
            iface.type_def(TypeDef::resource(resource, funcs));
        }

        for func in free_funcs {
            iface.function(func);
        }

        // `use` statements for types borrowed from other interfaces.
        let mut applied: BTreeSet<(String, String)> = BTreeSet::new();
        for (source_fq, item) in uses {
            let target = use_target(fq, &source_fq);
            if applied.insert((target.clone(), item.clone())) {
                iface.use_type(target, item, None);
            }
        }

        Ok(iface)
    }

    /// Reconstruct one resource method/static/constructor as a `ResourceFunc`.
    fn build_resource_func(
        &self,
        func: &crate::component_model::CmFunctionInfo,
        kind: ResKind,
        member: &str,
        fq: &str,
        uses: &mut Vec<(String, String)>,
    ) -> Result<ResourceFunc, WitEmitError> {
        let mut rf = match kind {
            ResKind::Constructor => ResourceFunc::constructor(),
            ResKind::Method => ResourceFunc::method(member.to_string(), func.is_async),
            ResKind::Static => ResourceFunc::static_(member.to_string(), func.is_async),
        };
        // Instance methods take the resource handle as an implicit `self`, which
        // WIT omits from the parameter list.
        let skip_self = usize::from(matches!(kind, ResKind::Method));
        let mut params = Params::empty();
        for (_, cm_name, ty) in func.params.iter().skip(skip_self) {
            params.push(cm_name.clone(), self.map_ast_type(ty, fq, uses)?);
        }
        rf.set_params(params);
        if !matches!(kind, ResKind::Constructor) {
            rf.set_result(self.map_cm_result(&func.return_type, func.is_async, fq, uses)?);
        }
        Ok(rf)
    }

    /// Map a CM function's return type to a WIT result, unwrapping the `Future`
    /// wrapper on async functions and collapsing unit to "no result".
    fn map_cm_result(
        &self,
        ret: &Option<crate::ast::Type>,
        is_async: bool,
        fq: &str,
        uses: &mut Vec<(String, String)>,
    ) -> Result<Option<Type>, WitEmitError> {
        use crate::ast::Type as AstType;
        let Some(ty) = ret else {
            return Ok(None);
        };
        let inner: Option<&AstType> = if is_async {
            match ty {
                AstType::Generic(g) if g.name == "Future" => g.args.first(),
                other => Some(other),
            }
        } else {
            Some(ty)
        };
        match inner {
            None => Ok(None),
            Some(t) if is_ast_unit(t) => Ok(None),
            Some(t) => Ok(Some(self.map_ast_type(t, fq, uses)?)),
        }
    }

    /// Resolve the WIT name of a CM-defined named type, using the registry's
    /// `cm_name` (which preserves acronym casing, e.g. `DNS-error-payload`) so
    /// references match their definitions. Records a cross-interface `use`.
    fn cm_type_name(
        &self,
        named: &crate::ast::NamedType,
        current_fq: &str,
        uses: &mut Vec<(String, String)>,
    ) -> String {
        let cm_name = self
            .sem
            .cm_interface_registry()
            .zip(named.source_interface.as_deref())
            .and_then(|(reg, src)| {
                reg.get_struct_cm_name_by_source(src, &named.name)
                    .or_else(|| reg.get_variant_cm_name_by_source(src, &named.name))
                    .or_else(|| reg.get_enum_cm_name_by_source(src, &named.name))
                    .or_else(|| reg.get_flags_cm_name_by_source(src, &named.name))
                    .or_else(|| reg.get_resource_cm_name_by_source(src, &named.name))
            })
            .map(str::to_string)
            .unwrap_or_else(|| to_kebab(&named.name));
        if let Some(src) = &named.source_interface
            && src != current_fq
        {
            uses.push((src.clone(), cm_name.clone()));
        }
        cm_name
    }

    /// Map an AST type from a CM signature to its WIT type, recording any
    /// cross-interface named-type references in `uses` as `(source_fq, item)`.
    /// Structural shapes route through the shared [`assemble`] rule; leaves
    /// render here.
    fn map_ast_type(
        &self,
        ty: &crate::ast::Type,
        current_fq: &str,
        uses: &mut Vec<(String, String)>,
    ) -> Result<Type, WitEmitError> {
        match classify_ast(ty) {
            CmShape::Leaf => self.map_ast_leaf(ty, current_fq, uses),
            shape => assemble(shape, |child| self.map_ast_type(&child, current_fq, uses)),
        }
    }

    /// Render an AST leaf: primitive, named CM type, or `&Resource` borrow.
    fn map_ast_leaf(
        &self,
        ty: &crate::ast::Type,
        current_fq: &str,
        uses: &mut Vec<(String, String)>,
    ) -> Result<Type, WitEmitError> {
        use crate::ast::Type as AstType;
        match ty {
            AstType::Named(named) => match primitive_by_name(&named.name) {
                Some(prim) => Ok(prim),
                None => Ok(Type::named(self.cm_type_name(named, current_fq, uses))),
            },
            // `&Resource` becomes `borrow<resource>`; other references are
            // transparent (already peeled by `classify_ast`).
            AstType::Reference(inner) | AstType::MutReference(inner) => {
                if let AstType::Named(named) = inner.as_ref() {
                    Ok(Type::borrow(self.cm_type_name(named, current_fq, uses)))
                } else {
                    self.map_ast_type(inner, current_fq, uses)
                }
            }
            other => Err(WitEmitError::UnrepresentableType {
                description: format!("CM signature type `{other:?}` has no WIT form"),
            }),
        }
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
            // `nt.type_id` is the newtype itself; emit an alias to its base, not
            // a self-referential `type x = x`.
            let base_id = match self.types.get(nt.type_id) {
                ResolvedType::Newtype { base_type, .. } => *base_type,
                _ => nt.type_id,
            };
            let base = self.map_type(base_id)?;
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
    /// referenced user types for later `TypeDef` emission. Structural shapes
    /// route through the shared [`assemble`] rule; leaves render here.
    fn map_type(&mut self, type_id: TypeId) -> Result<Type, WitEmitError> {
        match self.classify_resolved(type_id) {
            CmShape::Leaf => self.map_resolved_leaf(type_id),
            shape => assemble(shape, |child| self.map_type(child)),
        }
    }

    /// Classify a resolved type into its CM structural shape. `AsyncCall<T>`
    /// is transparent; a `&Resource` is left as a leaf (rendered `borrow<R>`),
    /// while other references are transparent at the value-semantics boundary.
    fn classify_resolved(&self, id: TypeId) -> CmShape<TypeId> {
        match self.types.get(id) {
            ResolvedType::BuiltinArray(inner) => CmShape::List(*inner),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                if self.is_resource(*inner) {
                    CmShape::Leaf
                } else {
                    self.classify_resolved(*inner)
                }
            }
            ResolvedType::GenericInstance {
                name, type_args, ..
            } => match name.as_str() {
                "Option" if type_args.len() == 1 => CmShape::Option(type_args[0]),
                "List" if type_args.len() == 1 => CmShape::List(type_args[0]),
                "Tuple" => CmShape::Tuple(type_args.clone()),
                "Result" if type_args.len() == 2 => CmShape::Result {
                    ok: self.non_unit(type_args[0]),
                    err: self.non_unit(type_args[1]),
                },
                "AsyncCall" if type_args.len() == 1 => self.classify_resolved(type_args[0]),
                _ => CmShape::Leaf,
            },
            ResolvedType::GenericResource {
                name, type_args, ..
            } => match name.as_str() {
                "Future" => CmShape::Future(type_args.first().copied()),
                "Stream" => CmShape::Stream(type_args.first().copied()),
                _ => CmShape::Leaf,
            },
            _ => CmShape::Leaf,
        }
    }

    /// Render a resolved leaf: primitive, named type, or handle. An owned
    /// resource becomes its bare WIT name (`own<R>`); a borrowed one becomes
    /// `borrow<R>`.
    fn map_resolved_leaf(&mut self, id: TypeId) -> Result<Type, WitEmitError> {
        match self.types.get(id) {
            ResolvedType::Primitive(p) => map_primitive(*p),
            ResolvedType::Struct { name, .. } if name == "String" => Ok(Type::String),
            ResolvedType::Struct { name, .. }
            | ResolvedType::Enum { name, .. }
            | ResolvedType::Variant { name, .. }
            | ResolvedType::Flags { name, .. }
            | ResolvedType::Newtype { name, .. } => Ok(self.named(name, id)),
            ResolvedType::Resource { name, .. } => Ok(Type::named(to_kebab(name))),
            ResolvedType::Ref(inner) | ResolvedType::MutRef(inner) => {
                let inner = *inner;
                if let ResolvedType::Resource { name, .. } = self.types.get(inner) {
                    Ok(Type::borrow(to_kebab(name)))
                } else {
                    self.map_type(inner)
                }
            }
            other => Err(WitEmitError::UnrepresentableType {
                description: describe_type(other),
            }),
        }
    }

    fn is_resource(&self, id: TypeId) -> bool {
        matches!(self.types.get(id), ResolvedType::Resource { .. })
    }

    /// A `Result` arm: a unit arm is absent (`_`).
    fn non_unit(&self, id: TypeId) -> Option<TypeId> {
        if matches!(self.types.get(id), ResolvedType::Unit) {
            None
        } else {
            Some(id)
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

/// A single-level structural classification of a CM-boundary type. Children
/// stay in the originating front-end's native representation (`T` is `TypeId`
/// for resolved types, `ast::Type` for CM-registry signatures) so the shared
/// [`assemble`] rule can recurse back through the same mapper. `Leaf` covers
/// primitives, named types, and handles — rendered by the front-end.
enum CmShape<T> {
    Option(T),
    List(T),
    Tuple(Vec<T>),
    Result { ok: Option<T>, err: Option<T> },
    Future(Option<T>),
    Stream(Option<T>),
    Leaf,
}

/// The one place the WIT structural constructors (`option` / `list` / `tuple`
/// / `result` / `future` / `stream`) are built, so the resolved-type and
/// CM-AST front-ends cannot drift in how a shape becomes WIT. `render` maps a
/// child in the front-end's native representation.
fn assemble<T>(
    shape: CmShape<T>,
    mut render: impl FnMut(T) -> Result<Type, WitEmitError>,
) -> Result<Type, WitEmitError> {
    Ok(match shape {
        CmShape::Leaf => unreachable!("leaves are rendered by the front-end, not assembled"),
        CmShape::Option(t) => Type::option(render(t)?),
        CmShape::List(t) => Type::list(render(t)?),
        CmShape::Tuple(ts) => {
            let mut elems = Vec::with_capacity(ts.len());
            for t in ts {
                elems.push(render(t)?);
            }
            Type::tuple(elems)
        }
        CmShape::Result { ok, err } => {
            let ok = ok.map(&mut render).transpose()?;
            let err = err.map(&mut render).transpose()?;
            match (ok, err) {
                (None, None) => Type::result_empty(),
                (Some(o), None) => Type::result_ok(o),
                (None, Some(e)) => Type::result_err(e),
                (Some(o), Some(e)) => Type::result_both(o, e),
            }
        }
        CmShape::Future(t) => Type::future(t.map(&mut render).transpose()?),
        CmShape::Stream(t) => Type::stream(t.map(&mut render).transpose()?),
    })
}

/// Classify a CM-signature AST type into its structural shape. `AsyncCall<T>`
/// is transparent; a `&Named` (a CM resource) is a leaf rendered `borrow<R>`,
/// while other references are transparent.
fn classify_ast(ty: &crate::ast::Type) -> CmShape<crate::ast::Type> {
    use crate::ast::Type as AstType;
    match ty {
        AstType::Tuple(elems) => CmShape::Tuple(elems.clone()),
        AstType::Reference(inner) | AstType::MutReference(inner) => {
            if matches!(inner.as_ref(), AstType::Named(_)) {
                CmShape::Leaf
            } else {
                classify_ast(inner)
            }
        }
        AstType::Generic(g) => match g.name.as_str() {
            "Option" if g.args.len() == 1 => CmShape::Option(g.args[0].clone()),
            "List" if g.args.len() == 1 => CmShape::List(g.args[0].clone()),
            "Tuple" => CmShape::Tuple(g.args.clone()),
            "Result" if g.args.len() == 2 => CmShape::Result {
                ok: non_unit_ast(&g.args[0]),
                err: non_unit_ast(&g.args[1]),
            },
            "Future" => CmShape::Future(g.args.first().cloned()),
            "Stream" => CmShape::Stream(g.args.first().cloned()),
            "AsyncCall" if g.args.len() == 1 => classify_ast(&g.args[0]),
            _ => CmShape::Leaf,
        },
        _ => CmShape::Leaf,
    }
}

/// A `Result` arm in AST form: a unit arm is absent (`_`).
fn non_unit_ast(ty: &crate::ast::Type) -> Option<crate::ast::Type> {
    if is_ast_unit(ty) {
        None
    } else {
        Some(ty.clone())
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

/// The kind of resource-associated function, encoded by the `#[cm]` fragment
/// prefix (`[method]` / `[static]` / `[constructor]`).
#[derive(Clone, Copy)]
enum ResKind {
    Method,
    Static,
    Constructor,
}

/// Classify a CM function name as a resource method/static/constructor,
/// returning `(kind, resource_cm_name, member_name)`. Free functions yield
/// `None`.
fn parse_resource_func(wasi_func_name: &str) -> Option<(ResKind, &str, &str)> {
    if let Some(resource) = wasi_func_name.strip_prefix("[constructor]") {
        Some((ResKind::Constructor, resource, ""))
    } else if let Some(rest) = wasi_func_name.strip_prefix("[method]") {
        let (resource, member) = rest.split_once('.')?;
        Some((ResKind::Method, resource, member))
    } else if let Some(rest) = wasi_func_name.strip_prefix("[static]") {
        let (resource, member) = rest.split_once('.')?;
        Some((ResKind::Static, resource, member))
    } else {
        None
    }
}

/// The parsed components of a CM interface FQ, e.g.
/// `wasi:cli/stdout@0.3.0` -> (`wasi`, `cli`, `stdout`, `0.3.0`).
struct FqParts {
    namespace: String,
    package: String,
    interface: String,
    version: String,
}

impl FqParts {
    fn parse(fq: &str) -> Option<Self> {
        let (path, version) = fq.split_once('@')?;
        let (ns_pkg, interface) = path.split_once('/')?;
        let (namespace, package) = ns_pkg.split_once(':')?;
        Some(Self {
            namespace: namespace.to_string(),
            package: package.to_string(),
            interface: interface.to_string(),
            version: version.to_string(),
        })
    }
}

/// The `use` target for a type defined in `source_fq` referenced from
/// `current_fq`: a bare interface name within the same package, else the full
/// `namespace:package/interface@version` path.
fn use_target(current_fq: &str, source_fq: &str) -> String {
    match (FqParts::parse(current_fq), FqParts::parse(source_fq)) {
        (Some(cur), Some(src)) if cur.namespace == src.namespace && cur.package == src.package => {
            src.interface
        }
        (_, Some(src)) => format!(
            "{}:{}/{}@{}",
            src.namespace, src.package, src.interface, src.version
        ),
        _ => source_fq.to_string(),
    }
}

/// Map a Wado primitive type name to its WIT type, if it names a primitive.
fn primitive_by_name(name: &str) -> Option<Type> {
    let ty = match name {
        "i8" => Type::S8,
        "i16" => Type::S16,
        "i32" => Type::S32,
        "i64" => Type::S64,
        "u8" => Type::U8,
        "u16" => Type::U16,
        "u32" => Type::U32,
        "u64" => Type::U64,
        "f32" => Type::F32,
        "f64" => Type::F64,
        "bool" => Type::Bool,
        "char" => Type::Char,
        "String" => Type::String,
        _ => return None,
    };
    Some(ty)
}

/// Whether an AST type is the unit type `()`.
fn is_ast_unit(ty: &crate::ast::Type) -> bool {
    match ty {
        crate::ast::Type::Tuple(elems) => elems.is_empty(),
        crate::ast::Type::Named(named) => named.name == "()",
        _ => false,
    }
}

/// Collect the source-interface FQs of every CM-defined named type referenced
/// by `ty` (recursing through generics, tuples, references, and function
/// types). Only `wasi:` / `core:` sources are CM interfaces worth importing.
fn collect_named_type_sources(ty: &crate::ast::Type, out: &mut Vec<String>) {
    use crate::ast::Type;
    match ty {
        Type::Named(named) => {
            if let Some(src) = &named.source_interface
                && (src.starts_with("wasi:") || src.starts_with("core:"))
            {
                out.push(src.clone());
            }
        }
        Type::Generic(generic) => {
            for arg in &generic.args {
                collect_named_type_sources(arg, out);
            }
        }
        Type::NamespacedGeneric(generic) => {
            for arg in &generic.args {
                collect_named_type_sources(arg, out);
            }
        }
        Type::Function(func) => {
            for param in &func.params {
                collect_named_type_sources(param, out);
            }
            collect_named_type_sources(&func.return_type, out);
        }
        Type::Tuple(elems) => {
            for elem in elems {
                collect_named_type_sources(elem, out);
            }
        }
        Type::Reference(inner) | Type::MutReference(inner) => {
            collect_named_type_sources(inner, out);
        }
        Type::TypePackSpread(_, _) | Type::Infer(_) | Type::Error(_) => {}
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
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    let mut prev_lower_or_digit = false;
    for (i, &ch) in chars.iter().enumerate() {
        if ch == '_' {
            if !out.ends_with('-') && !out.is_empty() {
                out.push('-');
            }
            prev_lower_or_digit = false;
            continue;
        }
        if ch.is_ascii_uppercase() {
            // Break before an uppercase letter that starts a new word: either
            // after a lowercase/digit (`myApi` -> `my-api`), or at the end of
            // an acronym run when the next char is lowercase
            // (`HTTPServer` -> `http-server`).
            let acronym_boundary = chars.get(i + 1).is_some_and(char::is_ascii_lowercase)
                && i > 0
                && chars[i - 1].is_ascii_uppercase();
            if (prev_lower_or_digit || acronym_boundary) && !out.is_empty() {
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
