//! WIT-to-IR transformation

use std::cell::RefCell;

use anyhow::Result;
use indexmap::IndexMap;
use wit_parser::{
    Function, FunctionKind, Handle, InterfaceId, Param, Resolve, Type, TypeDefKind, TypeId,
    TypeOwner, WorldId, WorldItem, WorldKey,
};

use crate::ir::{
    WadoEffect, WadoEnum, WadoEnumVariant, WadoField, WadoFlagMember, WadoFlags, WadoFunction,
    WadoImport, WadoModule, WadoNewtype, WadoParam, WadoResource, WadoStruct, WadoType,
    WadoTypeDef, WadoVariant, WadoVariantCase, WadoWorld, WadoWorldExport, WadoWorldImport,
};
use crate::naming::{to_snake_case, to_upper_camel_case};

pub struct Transformer<'a> {
    resolve: &'a Resolve,
    /// The interface currently being transformed (for cross-interface import detection)
    current_interface: RefCell<Option<InterfaceId>>,
    /// Accumulated foreign-type imports: `type_name` → wasi source path
    pending_imports: RefCell<IndexMap<String, String>>,
}

impl<'a> Transformer<'a> {
    #[must_use]
    pub fn new(resolve: &'a Resolve) -> Self {
        Self {
            resolve,
            current_interface: RefCell::new(None),
            pending_imports: RefCell::new(IndexMap::new()),
        }
    }

    /// Transform a WIT interface to a Wado module
    ///
    /// # Errors
    ///
    /// Returns an error if type transformation fails.
    pub fn transform_interface(&self, iface_id: InterfaceId) -> Result<WadoModule> {
        let iface = &self.resolve.interfaces[iface_id];
        let (namespace, pkg_name, iface_name, version) = self.get_interface_path(iface_id);

        let cm_interface = format!("{namespace}:{pkg_name}/{iface_name}@{version}");

        // Track current interface for cross-interface import detection
        *self.current_interface.borrow_mut() = Some(iface_id);
        self.pending_imports.borrow_mut().clear();

        let mut module = WadoModule::new(iface_name.clone(), version);

        // Transform types
        for (_, type_id) in &iface.types {
            if let Some(type_def) = self.transform_type_def(*type_id, &cm_interface)? {
                module.types.push(type_def);
            }
        }

        // Transform resources
        for (_, type_id) in &iface.types {
            if let Some(resource) = self.transform_resource(*type_id, &cm_interface)? {
                module.resources.push(resource);
            }
        }

        // Transform functions into an effect
        let functions: Vec<WadoFunction> = iface
            .functions
            .iter()
            .filter(|(_, func)| {
                matches!(
                    func.kind,
                    FunctionKind::Freestanding | FunctionKind::AsyncFreestanding
                )
            })
            .map(|(name, func)| self.transform_function(name, func, &cm_interface))
            .collect::<Result<Vec<_>>>()?;

        if !functions.is_empty() {
            module.effects.push(WadoEffect {
                name: to_upper_camel_case(&iface_name),
                doc_comment: iface.docs.contents.clone(),
                cm_interface,
                functions,
            });
        }

        // Collect cross-interface imports grouped by source path
        let raw_imports = self.pending_imports.borrow().clone();
        let mut imports_by_path: IndexMap<String, Vec<String>> = IndexMap::new();
        for (type_name, path) in raw_imports {
            imports_by_path.entry(path).or_default().push(type_name);
        }
        for (from_path, mut type_names) in imports_by_path {
            type_names.sort();
            module.imports.push(WadoImport {
                type_names,
                from_path,
            });
        }

        *self.current_interface.borrow_mut() = None;

        Ok(module)
    }

    /// Transform a WIT world to a `WadoWorld`
    ///
    /// # Errors
    ///
    /// Returns an error if type transformation fails.
    pub fn transform_world(&self, world_id: WorldId) -> Result<WadoWorld> {
        let world = &self.resolve.worlds[world_id];
        let world_name = world.name.clone();

        // Build canonical name like "wasi:http/proxy@0.3.0"
        let canonical_name = if let Some(pkg_id) = world.package {
            let pkg = &self.resolve.packages[pkg_id];
            let namespace = &pkg.name.namespace;
            let pkg_name = &pkg.name.name;
            if let Some(version) = &pkg.name.version {
                format!("{namespace}:{pkg_name}/{world_name}@{version}")
            } else {
                format!("{namespace}:{pkg_name}/{world_name}")
            }
        } else {
            world_name.clone()
        };

        let mut imports = Vec::new();
        let mut exports = Vec::new();

        for (key, item) in &world.imports {
            if let WorldItem::Interface { id, .. } = item {
                let iface = &self.resolve.interfaces[*id];
                let iface_name = match key {
                    WorldKey::Name(n) => n.clone(),
                    WorldKey::Interface(id) => self.get_interface_name(*id),
                };

                let functions: Vec<String> = iface
                    .functions
                    .iter()
                    .map(|(func_name, func)| {
                        self.format_world_import_function_name(func_name, func)
                    })
                    .collect();

                if !functions.is_empty() {
                    imports.push(WadoWorldImport {
                        effect_name: to_upper_camel_case(&iface_name),
                        functions,
                    });
                }
            }
        }

        for (key, item) in &world.exports {
            match item {
                WorldItem::Function(func) => {
                    let name = match key {
                        WorldKey::Name(n) => to_snake_case(n),
                        WorldKey::Interface(_) => continue,
                    };

                    let params = self.transform_params(&func.params)?;
                    let return_type = self.transform_result(func.result.as_ref())?;

                    exports.push(WadoWorldExport {
                        name,
                        is_async: matches!(func.kind, FunctionKind::AsyncFreestanding),
                        params,
                        return_type,
                    });
                }
                WorldItem::Interface { id, .. } => {
                    // When exporting an interface, export all its functions
                    let iface = &self.resolve.interfaces[*id];
                    for (func_name, func) in &iface.functions {
                        if matches!(
                            func.kind,
                            FunctionKind::Freestanding | FunctionKind::AsyncFreestanding
                        ) {
                            let params = self.transform_params(&func.params)?;
                            let return_type = self.transform_result(func.result.as_ref())?;

                            exports.push(WadoWorldExport {
                                name: to_snake_case(func_name),
                                is_async: matches!(func.kind, FunctionKind::AsyncFreestanding),
                                params,
                                return_type,
                            });
                        }
                    }
                }
                WorldItem::Type { .. } => {}
            }
        }

        Ok(WadoWorld {
            name: to_upper_camel_case(&world_name),
            canonical_name,
            doc_comment: world.docs.contents.clone(),
            imports,
            exports,
        })
    }

    /// Format a function name for use in world import lists.
    ///
    /// For resource methods, uses `ResourceName::method_name` format.
    /// For freestanding functions, uses plain `snake_case`.
    fn format_world_import_function_name(&self, func_name: &str, func: &Function) -> String {
        match &func.kind {
            FunctionKind::Constructor(resource_id) => {
                let resource_name = self.resolve.types[*resource_id]
                    .name
                    .as_ref()
                    .map_or_else(|| "Unknown".to_string(), |n| to_upper_camel_case(n));
                format!("{resource_name}::new")
            }
            FunctionKind::Method(resource_id)
            | FunctionKind::Static(resource_id)
            | FunctionKind::AsyncMethod(resource_id)
            | FunctionKind::AsyncStatic(resource_id) => {
                let resource_name = self.resolve.types[*resource_id]
                    .name
                    .as_ref()
                    .map_or_else(|| "Unknown".to_string(), |n| to_upper_camel_case(n));
                let method_name = func_name.split('.').next_back().unwrap_or(func_name);
                format!("{}::{}", resource_name, to_snake_case(method_name))
            }
            FunctionKind::Freestanding | FunctionKind::AsyncFreestanding => {
                to_snake_case(func_name)
            }
        }
    }

    fn get_interface_path(&self, iface_id: InterfaceId) -> (String, String, String, String) {
        let iface = &self.resolve.interfaces[iface_id];
        let iface_name = iface.name.clone().unwrap_or_else(|| "unknown".to_string());

        if let Some(pkg_id) = iface.package {
            let pkg = &self.resolve.packages[pkg_id];
            let namespace = pkg.name.namespace.clone();
            let pkg_name = pkg.name.name.clone();
            let version = pkg
                .name
                .version
                .as_ref()
                .map_or_else(|| "0.0.0".to_string(), ToString::to_string);
            (namespace, pkg_name, iface_name, version)
        } else {
            (
                "unknown".to_string(),
                "unknown".to_string(),
                iface_name,
                "0.0.0".to_string(),
            )
        }
    }

    fn get_interface_name(&self, iface_id: InterfaceId) -> String {
        let iface = &self.resolve.interfaces[iface_id];
        iface.name.clone().unwrap_or_else(|| "unknown".to_string())
    }

    fn transform_function(
        &self,
        name: &str,
        func: &Function,
        cm_interface: &str,
    ) -> Result<WadoFunction> {
        let cm_attr = format!("{cm_interface}#{name}");

        let params = self.transform_params(&func.params)?;
        let return_type = self.transform_result(func.result.as_ref())?;

        // Check if this is an async function
        let is_async = matches!(func.kind, FunctionKind::AsyncFreestanding);

        Ok(WadoFunction {
            name: to_snake_case(name),
            doc_comment: func.docs.contents.clone(),
            cm_attr,
            params,
            return_type,
            is_async,
            never_returns: false,
        })
    }

    fn transform_params(&self, params: &[Param]) -> Result<Vec<WadoParam>> {
        params
            .iter()
            .map(|param| {
                Ok(WadoParam {
                    name: to_snake_case(&param.name),
                    ty: self.transform_type(param.ty)?,
                    wit_name: param.name.clone(),
                })
            })
            .collect()
    }

    fn transform_result(&self, result: Option<&Type>) -> Result<Option<WadoType>> {
        match result {
            Some(ty) => Ok(Some(self.transform_type(*ty)?)),
            None => Ok(None),
        }
    }

    fn transform_type(&self, ty: Type) -> Result<WadoType> {
        match ty {
            Type::Bool => Ok(WadoType::Bool),
            Type::U8 => Ok(WadoType::U8),
            Type::U16 => Ok(WadoType::U16),
            Type::U32 => Ok(WadoType::U32),
            Type::U64 => Ok(WadoType::U64),
            Type::S8 => Ok(WadoType::I8),
            Type::S16 => Ok(WadoType::I16),
            Type::S32 => Ok(WadoType::I32),
            Type::S64 => Ok(WadoType::I64),
            Type::F32 => Ok(WadoType::F32),
            Type::F64 => Ok(WadoType::F64),
            Type::Char => Ok(WadoType::Char),
            Type::String => Ok(WadoType::String),
            Type::Id(type_id) => self.transform_type_id(type_id),
            Type::ErrorContext => Ok(WadoType::Named("ErrorContext".to_string())),
        }
    }

    /// Get the import path for an interface (with `.wado` extension),
    /// e.g. `"wasi:clocks/system_clock.wado"`. The WIT kebab-case
    /// interface name is converted to Wado's `snake_case` filename
    /// convention (matching the per-interface file emitted by
    /// `main.rs::run_directory_mode`).
    fn get_wasi_path_for_interface(&self, iface_id: InterfaceId) -> String {
        let iface = &self.resolve.interfaces[iface_id];
        let iface_name = iface.name.as_deref().unwrap_or("unknown");
        let file_stem = crate::naming::to_snake_case(iface_name);
        if let Some(pkg_id) = iface.package {
            let pkg = &self.resolve.packages[pkg_id];
            let namespace = &pkg.name.namespace;
            let pkg_name = &pkg.name.name;
            format!("{namespace}:{pkg_name}/{file_stem}.wado")
        } else {
            format!("{file_stem}.wado")
        }
    }

    fn transform_type_id(&self, type_id: TypeId) -> Result<WadoType> {
        let ty = &self.resolve.types[type_id];

        // If the type has a name, return it as a named type
        if let Some(name) = &ty.name {
            let wado_name = to_upper_camel_case(name);
            // Check if this type belongs to a different interface — record as import
            if let TypeOwner::Interface(owner_iface) = ty.owner {
                let current = *self.current_interface.borrow();
                if current != Some(owner_iface) {
                    let source_path = self.get_wasi_path_for_interface(owner_iface);
                    self.pending_imports
                        .borrow_mut()
                        .insert(wado_name.clone(), source_path);
                }
            }
            return Ok(WadoType::Named(wado_name));
        }

        // Anonymous types - inline them
        match &ty.kind {
            TypeDefKind::List(inner) => Ok(WadoType::Array(Box::new(self.transform_type(*inner)?))),
            TypeDefKind::Option(inner) => {
                Ok(WadoType::Option(Box::new(self.transform_type(*inner)?)))
            }
            TypeDefKind::Result(r) => {
                let ok =
                    r.ok.map(|t| self.transform_type(t))
                        .transpose()?
                        .map(Box::new);
                let err = r
                    .err
                    .map(|t| self.transform_type(t))
                    .transpose()?
                    .map(Box::new);
                Ok(WadoType::Result { ok, err })
            }
            TypeDefKind::Tuple(t) => {
                let types = t
                    .types
                    .iter()
                    .map(|ty| self.transform_type(*ty))
                    .collect::<Result<Vec<_>>>()?;
                Ok(WadoType::Tuple(types))
            }
            TypeDefKind::Stream(inner) => {
                // Stream is Option<Type> in wit-parser
                let element = inner
                    .as_ref()
                    .map(|t| self.transform_type(*t))
                    .transpose()?
                    .unwrap_or(WadoType::U8);
                Ok(WadoType::Stream(Box::new(element)))
            }
            TypeDefKind::Future(inner) => {
                // Future is Option<Type> in wit-parser
                let payload = inner
                    .as_ref()
                    .map(|t| self.transform_type(*t))
                    .transpose()?
                    .unwrap_or(WadoType::Tuple(vec![]));
                Ok(WadoType::Future(Box::new(payload)))
            }
            TypeDefKind::Handle(h) => match h {
                Handle::Borrow(id) => Ok(WadoType::Borrow(Box::new(self.transform_type_id(*id)?))),
                Handle::Own(id) => self.transform_type_id(*id),
            },
            TypeDefKind::Type(inner) => self.transform_type(*inner),
            _ => Ok(WadoType::Named("Unknown".to_string())),
        }
    }

    fn transform_type_def(
        &self,
        type_id: TypeId,
        cm_interface: &str,
    ) -> Result<Option<WadoTypeDef>> {
        let ty = &self.resolve.types[type_id];

        // Skip anonymous types and resources
        let name = match &ty.name {
            Some(n) => to_upper_camel_case(n),
            None => return Ok(None),
        };

        let cm_attr = format!("{}#{}", cm_interface, ty.name.as_ref().unwrap());
        let doc_comment = ty.docs.contents.clone();

        let def = match &ty.kind {
            TypeDefKind::Enum(e) => {
                WadoTypeDef::Enum(Self::transform_enum(e, name, doc_comment, cm_attr))
            }
            TypeDefKind::Flags(f) => {
                WadoTypeDef::Flags(Self::transform_flags(f, name, doc_comment, cm_attr))
            }
            TypeDefKind::Record(r) => {
                WadoTypeDef::Struct(self.transform_record(r, name, doc_comment, cm_attr)?)
            }
            TypeDefKind::Variant(v) => {
                WadoTypeDef::Variant(self.transform_variant(v, name, doc_comment, cm_attr)?)
            }
            TypeDefKind::Type(inner) => WadoTypeDef::Newtype(WadoNewtype {
                name,
                cm_attr: Some(cm_attr),
                target: self.transform_type(*inner)?,
            }),
            TypeDefKind::Tuple(t) => WadoTypeDef::Newtype(WadoNewtype {
                name,
                cm_attr: Some(cm_attr),
                target: WadoType::Tuple(
                    t.types
                        .iter()
                        .map(|ty| self.transform_type(*ty))
                        .collect::<Result<Vec<_>>>()?,
                ),
            }),
            TypeDefKind::List(inner) => WadoTypeDef::Newtype(WadoNewtype {
                name,
                cm_attr: Some(cm_attr),
                target: WadoType::Array(Box::new(self.transform_type(*inner)?)),
            }),
            _ => return Ok(None),
        };
        Ok(Some(def))
    }

    fn transform_enum(
        e: &wit_parser::Enum,
        name: String,
        doc_comment: Option<String>,
        cm_attr: String,
    ) -> WadoEnum {
        let variants = e
            .cases
            .iter()
            .map(|case| WadoEnumVariant {
                name: to_upper_camel_case(&case.name),
                doc_comment: case.docs.contents.clone(),
                cm_attr: Some(case.name.clone()),
            })
            .collect();
        WadoEnum {
            name,
            doc_comment,
            cm_attr: Some(cm_attr),
            variants,
        }
    }

    fn transform_flags(
        f: &wit_parser::Flags,
        name: String,
        doc_comment: Option<String>,
        cm_attr: String,
    ) -> WadoFlags {
        let flags = f
            .flags
            .iter()
            .map(|flag| WadoFlagMember {
                name: to_upper_camel_case(&flag.name),
                doc_comment: flag.docs.contents.clone(),
                cm_attr: flag.name.clone(),
            })
            .collect();
        WadoFlags {
            name,
            doc_comment,
            cm_attr: Some(cm_attr),
            flags,
        }
    }

    fn transform_record(
        &self,
        r: &wit_parser::Record,
        name: String,
        doc_comment: Option<String>,
        cm_attr: String,
    ) -> Result<WadoStruct> {
        let fields = r
            .fields
            .iter()
            .map(|field| {
                Ok(WadoField {
                    name: to_snake_case(&field.name),
                    ty: self.transform_type(field.ty)?,
                    doc_comment: field.docs.contents.clone(),
                    cm_attr: field.name.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(WadoStruct {
            name,
            doc_comment,
            cm_attr: Some(cm_attr),
            fields,
        })
    }

    fn transform_variant(
        &self,
        v: &wit_parser::Variant,
        name: String,
        doc_comment: Option<String>,
        cm_attr: String,
    ) -> Result<WadoVariant> {
        let cases = v
            .cases
            .iter()
            .map(|case| {
                Ok(WadoVariantCase {
                    name: to_upper_camel_case(&case.name),
                    payload: case.ty.map(|t| self.transform_type(t)).transpose()?,
                    doc_comment: case.docs.contents.clone(),
                    cm_attr: Some(case.name.clone()),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(WadoVariant {
            name,
            doc_comment,
            cm_attr: Some(cm_attr),
            cases,
        })
    }

    fn transform_resource(
        &self,
        type_id: TypeId,
        cm_interface: &str,
    ) -> Result<Option<WadoResource>> {
        let ty = &self.resolve.types[type_id];

        if !matches!(ty.kind, TypeDefKind::Resource) {
            return Ok(None);
        }

        let name = match &ty.name {
            Some(n) => to_upper_camel_case(n),
            None => return Ok(None),
        };

        let cm_attr = format!("{}#{}", cm_interface, ty.name.as_ref().unwrap());

        // Find methods for this resource
        let mut methods = Vec::new();
        let resource_wit_name = ty.name.as_ref().unwrap();

        // Look for methods in the owning interface
        if let TypeOwner::Interface(iface_id) = ty.owner {
            let iface = &self.resolve.interfaces[iface_id];
            for (func_name, func) in &iface.functions {
                // Determine the method kind prefix for the WASI attribute
                // Handle both sync and async method variants
                let (method_kind, is_async) = match &func.kind {
                    FunctionKind::Constructor(resource_id) if *resource_id == type_id => {
                        (Some("[constructor]"), false)
                    }
                    FunctionKind::Method(resource_id) if *resource_id == type_id => {
                        (Some("[method]"), false)
                    }
                    FunctionKind::Static(resource_id) if *resource_id == type_id => {
                        (Some("[static]"), false)
                    }
                    FunctionKind::AsyncMethod(resource_id) if *resource_id == type_id => {
                        (Some("[method]"), true)
                    }
                    FunctionKind::AsyncStatic(resource_id) if *resource_id == type_id => {
                        (Some("[static]"), true)
                    }
                    _ => (None, false),
                };

                if let Some(kind_prefix) = method_kind {
                    // Build WASI attribute and extract method name based on kind
                    let (method_attr, method_name) = if kind_prefix == "[constructor]" {
                        // Constructor: wasi attr is "interface#[constructor]resource-name"
                        // Wado name is "new"
                        (
                            format!("{cm_interface}#[constructor]{resource_wit_name}"),
                            "new".to_string(),
                        )
                    } else {
                        // Method/Static: extract just the method name after the dot
                        let raw_method = func_name.split('.').next_back().unwrap_or(func_name);
                        let method_attr =
                            format!("{cm_interface}#{kind_prefix}{resource_wit_name}.{raw_method}");
                        (method_attr, to_snake_case(raw_method))
                    };

                    methods.push(WadoFunction {
                        name: method_name,
                        doc_comment: func.docs.contents.clone(),
                        cm_attr: method_attr,
                        params: self.transform_params(&func.params)?,
                        return_type: self.transform_result(func.result.as_ref())?,
                        is_async,
                        never_returns: false,
                    });
                }
            }
        }

        Ok(Some(WadoResource {
            name,
            doc_comment: ty.docs.contents.clone(),
            cm_attr,
            methods,
        }))
    }
}
