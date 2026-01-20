//! WIT-to-IR transformation

use anyhow::Result;
use wit_parser::{
    Function, FunctionKind, Handle, InterfaceId, Resolve, Type, TypeDefKind, TypeId, TypeOwner,
    WorldId, WorldItem, WorldKey,
};

use crate::ir::{
    WadoEffect, WadoEnum, WadoEnumVariant, WadoField, WadoFlagMember, WadoFlags, WadoFunction,
    WadoModule, WadoParam, WadoResource, WadoStruct, WadoType, WadoTypeAlias, WadoTypeDef,
    WadoVariant, WadoVariantCase, WadoWorld, WadoWorldExport, WadoWorldImport,
};
use crate::naming::{escape_keyword, to_snake_case, to_upper_camel_case};

pub struct Transformer<'a> {
    resolve: &'a Resolve,
}

impl<'a> Transformer<'a> {
    #[must_use] 
    pub fn new(resolve: &'a Resolve) -> Self {
        Self { resolve }
    }

    /// Transform a WIT interface to a Wado module
    ///
    /// # Errors
    ///
    /// Returns an error if type transformation fails.
    pub fn transform_interface(&self, iface_id: InterfaceId) -> Result<WadoModule> {
        let iface = &self.resolve.interfaces[iface_id];
        let (pkg_name, iface_name, version) = self.get_interface_path(iface_id);

        let wasi_interface = format!("wasi:{pkg_name}/{iface_name}@{version}");

        let mut module = WadoModule::new(iface_name.clone(), version.clone());

        // Transform types
        for (_, type_id) in &iface.types {
            if let Some(type_def) = self.transform_type_def(*type_id, &wasi_interface)? {
                module.types.push(type_def);
            }
        }

        // Transform resources
        for (_, type_id) in &iface.types {
            if let Some(resource) = self.transform_resource(*type_id, &wasi_interface)? {
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
            .map(|(name, func)| self.transform_function(name, func, &wasi_interface))
            .collect::<Result<Vec<_>>>()?;

        if !functions.is_empty() {
            module.effects.push(WadoEffect {
                name: to_upper_camel_case(&iface_name),
                doc_comment: iface.docs.contents.clone(),
                wasi_interface: wasi_interface.clone(),
                functions,
            });
        }

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

        let mut imports = Vec::new();
        let mut exports = Vec::new();

        for (key, item) in &world.imports {
            if let WorldItem::Interface { id, .. } = item {
                let iface = &self.resolve.interfaces[*id];
                let iface_name = match key {
                    WorldKey::Name(n) => n.clone(),
                    WorldKey::Interface(id) => self.get_interface_name(*id),
                };

                let functions: Vec<String> =
                    iface.functions.keys().map(|n| to_snake_case(n)).collect();

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
                WorldItem::Type(_) => {}
            }
        }

        Ok(WadoWorld {
            name: to_upper_camel_case(&world_name),
            doc_comment: world.docs.contents.clone(),
            imports,
            exports,
        })
    }

    fn get_interface_path(&self, iface_id: InterfaceId) -> (String, String, String) {
        let iface = &self.resolve.interfaces[iface_id];
        let iface_name = iface.name.clone().unwrap_or_else(|| "unknown".to_string());

        if let Some(pkg_id) = iface.package {
            let pkg = &self.resolve.packages[pkg_id];
            let pkg_name = pkg.name.name.clone();
            let version = pkg
                .name
                .version
                .as_ref()
                .map_or_else(|| "0.0.0".to_string(), ToString::to_string);
            (pkg_name, iface_name, version)
        } else {
            ("unknown".to_string(), iface_name, "0.0.0".to_string())
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
        wasi_interface: &str,
    ) -> Result<WadoFunction> {
        let wasi_attr = format!("{wasi_interface}#{name}");

        let params = self.transform_params(&func.params)?;
        let return_type = self.transform_result(func.result.as_ref())?;

        // Check if this is an async function
        let is_async = matches!(func.kind, FunctionKind::AsyncFreestanding);

        Ok(WadoFunction {
            name: to_snake_case(name),
            doc_comment: func.docs.contents.clone(),
            wasi_attr,
            params,
            return_type,
            is_async,
            never_returns: false,
        })
    }

    fn transform_params(&self, params: &[(String, Type)]) -> Result<Vec<WadoParam>> {
        params
            .iter()
            .map(|(name, ty)| {
                Ok(WadoParam {
                    name: escape_keyword(name),
                    ty: self.transform_type(*ty)?,
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

    fn transform_type_id(&self, type_id: TypeId) -> Result<WadoType> {
        let ty = &self.resolve.types[type_id];

        // If the type has a name, return it as a named type
        if let Some(name) = &ty.name {
            return Ok(WadoType::Named(to_upper_camel_case(name)));
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
        wasi_interface: &str,
    ) -> Result<Option<WadoTypeDef>> {
        let ty = &self.resolve.types[type_id];

        // Skip anonymous types and resources
        let name = match &ty.name {
            Some(n) => to_upper_camel_case(n),
            None => return Ok(None),
        };

        let wasi_attr = format!("{}#{}", wasi_interface, ty.name.as_ref().unwrap());

        match &ty.kind {
            TypeDefKind::Enum(e) => {
                let variants = e
                    .cases
                    .iter()
                    .map(|case| WadoEnumVariant {
                        name: to_upper_camel_case(&case.name),
                        doc_comment: case.docs.contents.clone(),
                    })
                    .collect();

                Ok(Some(WadoTypeDef::Enum(WadoEnum {
                    name,
                    doc_comment: ty.docs.contents.clone(),
                    wasi_attr: Some(wasi_attr),
                    variants,
                })))
            }
            TypeDefKind::Flags(f) => {
                let flags = f
                    .flags
                    .iter()
                    .map(|flag| WadoFlagMember {
                        name: to_upper_camel_case(&flag.name),
                        doc_comment: flag.docs.contents.clone(),
                    })
                    .collect();

                Ok(Some(WadoTypeDef::Flags(WadoFlags {
                    name,
                    doc_comment: ty.docs.contents.clone(),
                    wasi_attr: Some(wasi_attr),
                    flags,
                })))
            }
            TypeDefKind::Record(r) => {
                let fields = r
                    .fields
                    .iter()
                    .map(|field| {
                        Ok(WadoField {
                            name: escape_keyword(&field.name),
                            ty: self.transform_type(field.ty)?,
                            doc_comment: field.docs.contents.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;

                Ok(Some(WadoTypeDef::Struct(WadoStruct {
                    name,
                    doc_comment: ty.docs.contents.clone(),
                    wasi_attr: Some(wasi_attr),
                    fields,
                })))
            }
            TypeDefKind::Variant(v) => {
                let cases = v
                    .cases
                    .iter()
                    .map(|case| {
                        Ok(WadoVariantCase {
                            name: to_upper_camel_case(&case.name),
                            payload: case.ty.map(|t| self.transform_type(t)).transpose()?,
                            doc_comment: case.docs.contents.clone(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;

                Ok(Some(WadoTypeDef::Variant(WadoVariant {
                    name,
                    doc_comment: ty.docs.contents.clone(),
                    wasi_attr: Some(wasi_attr),
                    cases,
                })))
            }
            // Resources handled separately by transform_resource
            TypeDefKind::Type(inner) => {
                // Type alias (e.g., `type instant = u64;` in WIT)
                let target = self.transform_type(*inner)?;
                Ok(Some(WadoTypeDef::TypeAlias(WadoTypeAlias {
                    name,
                    wasi_attr: Some(wasi_attr),
                    target,
                })))
            }
            _ => Ok(None),
        }
    }

    fn transform_resource(
        &self,
        type_id: TypeId,
        wasi_interface: &str,
    ) -> Result<Option<WadoResource>> {
        let ty = &self.resolve.types[type_id];

        if !matches!(ty.kind, TypeDefKind::Resource) {
            return Ok(None);
        }

        let name = match &ty.name {
            Some(n) => to_upper_camel_case(n),
            None => return Ok(None),
        };

        let wasi_attr = format!("{}#{}", wasi_interface, ty.name.as_ref().unwrap());

        // Find methods for this resource
        let mut methods = Vec::new();

        // Look for methods in the owning interface
        if let TypeOwner::Interface(iface_id) = ty.owner {
            let iface = &self.resolve.interfaces[iface_id];
            for (func_name, func) in &iface.functions {
                match &func.kind {
                    FunctionKind::Method(resource_id) | FunctionKind::Static(resource_id)
                        if *resource_id == type_id =>
                    {
                        let method_attr = format!(
                            "{}#[method]{}.{}",
                            wasi_interface,
                            ty.name.as_ref().unwrap(),
                            func_name
                        );
                        methods.push(WadoFunction {
                            name: to_snake_case(func_name),
                            doc_comment: func.docs.contents.clone(),
                            wasi_attr: method_attr,
                            params: self.transform_params(&func.params)?,
                            return_type: self.transform_result(func.result.as_ref())?,
                            is_async: false,
                            never_returns: false,
                        });
                    }
                    _ => {}
                }
            }
        }

        Ok(Some(WadoResource {
            name,
            doc_comment: ty.docs.contents.clone(),
            wasi_attr,
            methods,
        }))
    }
}
