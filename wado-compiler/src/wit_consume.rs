//! Build a Wado AST module from a decoded CM component's WIT type.
//!
//! The consumer side of WIT interoperability: turns the `wit_parser::Resolve`
//! from `wit_component::decode` into an `ast::Module` of `#[cm(...)]`-annotated
//! interfaces, functions, and types — the shape `wado-from-idl` emits for the
//! stdlib, but built directly as AST. The normal frontend then handles it.
//!
//! Type mapping reuses [`crate::wit_emit`]'s `CmShape` so producer and consumer
//! cannot drift on how `option`/`list`/`tuple`/`result` are structured.

use crate::ast::{AstId, AstIdSpace};
use crate::ast::{
    AttrArg, Attribute, CmBoundary, CmImport, EnumCase, EnumDecl, FlagsDecl, FlagsVariant,
    GenericType, InnerAttribute, InterfaceDecl, InterfaceMethod, Item, Module, NamedType, Newtype,
    Param, SelfKind, StructDecl, StructField, Type, VariantCase, VariantDecl, Visibility,
};
use crate::token::Span;
use crate::wit_emit::CmShape;
use heck::{ToSnakeCase, ToUpperCamelCase};
use wit_parser::{Resolve, Type as WitType, TypeDefKind, TypeId, TypeOwner, WorldId, WorldItem};

/// Inner-attribute name carrying a component's host-leaf import FQs on its
/// synthesized module, so effect reconstruction can recover them from the AST
/// alone (see [Effect Reconstruction from CM Component Imports]).
pub const CM_HOST_IMPORTS_ATTR: &str = "cm_host_imports";

/// The Wado bindings synthesized from one decoded imported component.
pub struct ComponentBindings {
    /// The AST module declaring the component's exported interfaces and types.
    pub module: Module,
    /// FQ names of every exported interface (drives import-plan classification).
    pub interface_fqs: Vec<String>,
    /// FQ names of the interfaces the component itself imports — its host-leaf
    /// capabilities (WASI, etc.). Effect reconstruction maps these onto the
    /// consumer's effects; a purely-computational component imports none.
    pub host_leaf_imports: Vec<String>,
}

/// Read the host-leaf import FQs a component-binding module carries in its
/// [`CM_HOST_IMPORTS_ATTR`] inner attribute. Empty for a non-component module
/// or a component that imports nothing.
pub fn module_host_leaf_imports(module: &Module) -> Vec<String> {
    module
        .inner_attributes
        .iter()
        .find(|a| a.name == CM_HOST_IMPORTS_ATTR)
        .map(|a| {
            a.args
                .iter()
                .filter_map(|arg| {
                    if let AttrArg::Str(s) = arg {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Build a Wado AST module from a component's decoded `Resolve` + target world.
///
/// # Errors
/// Returns a message naming the offending shape if the component exports a WIT
/// construct not yet supported for import (resources/handles, world-level
/// function exports, etc.).
pub fn build_bindings(resolve: &Resolve, world: WorldId) -> Result<ComponentBindings, String> {
    let mut b = Builder::new();
    let mut interface_fqs = Vec::new();

    for (_, item) in &resolve.worlds[world].exports {
        match item {
            WorldItem::Interface { id, .. } => {
                let fq = interface_fq(resolve, *id);
                b.emit_interface(resolve, *id, &fq);
                interface_fqs.push(fq);
            }
            WorldItem::Function(f) => {
                return Err(format!(
                    "importing a component that exports the world-level function `{}` is not yet \
                     supported (only interface exports are)",
                    f.name
                ));
            }
            WorldItem::Type { .. } => {
                return Err(
                    "importing a component that exports a world-level type is not yet supported"
                        .to_string(),
                );
            }
        }
    }

    if !b.errors.is_empty() {
        return Err(b.errors.join("; "));
    }

    // The component's own imports are its host-leaf capabilities. Collect every
    // imported interface FQ so effect reconstruction can map them onto the
    // consumer's effects; imported world-level functions/types are not a v1
    // effect surface and are ignored.
    let host_leaf_imports: Vec<String> = resolve.worlds[world]
        .imports
        .iter()
        .filter_map(|(_, item)| match item {
            WorldItem::Interface { id, .. } => Some(interface_fq(resolve, *id)),
            WorldItem::Function(_) | WorldItem::Type { .. } => None,
        })
        .collect();

    // Carry the host-leaf set on the synthesized module so the registration
    // path (`fold_component_interfaces`) recovers it from the AST alone.
    let inner_attributes = if host_leaf_imports.is_empty() {
        Vec::new()
    } else {
        vec![InnerAttribute {
            name: CM_HOST_IMPORTS_ATTR.to_string(),
            args: host_leaf_imports.iter().cloned().map(AttrArg::Str).collect(),
            span: syn(),
        }]
    };

    let module = Module::with_metadata(
        b.items,
        inner_attributes,
        None,
        None,
        crate::hashmap::IndexSet::default(),
        b.space,
        b.count,
        false,
    );
    Ok(ComponentBindings {
        module,
        interface_fqs,
        host_leaf_imports,
    })
}

/// Allocates dense, module-local `AstId`s while building the synthesized module.
struct Builder {
    space: AstIdSpace,
    count: u32,
    items: Vec<Item>,
    errors: Vec<String>,
}

impl Builder {
    fn new() -> Self {
        Self {
            space: AstIdSpace::next(),
            count: 0,
            items: Vec::new(),
            errors: Vec::new(),
        }
    }

    fn id(&mut self) -> AstId {
        let id = AstId::new(self.space, self.count);
        self.count += 1;
        id
    }

    /// `#[cm("ns:pkg/iface@ver[#frag]")]` as an interface-import boundary.
    fn cm_import_attr(&self, fq: &str, frag: Option<&str>) -> Attribute {
        let mut cm = CmImport::parse(fq).expect("interface FQ is well-formed");
        cm.function = frag.map(str::to_string);
        Attribute {
            name: "cm".to_string(),
            args: Vec::new(),
            cm_boundary: Some(CmBoundary::Import(cm)),
            span: syn(),
        }
    }

    /// `#[cm("kebab-name")]` as a bare CM-name boundary (fields, cases, flags).
    fn cm_name_attr(&self, name: &str) -> Attribute {
        Attribute {
            name: "cm".to_string(),
            args: Vec::new(),
            cm_boundary: Some(CmBoundary::Name(name.to_string())),
            span: syn(),
        }
    }

    fn emit_interface(&mut self, resolve: &Resolve, iface_id: wit_parser::InterfaceId, fq: &str) {
        let iface = &resolve.interfaces[iface_id];

        // Named types first, so signatures can reference them.
        for (_, type_id) in &iface.types {
            self.emit_type_def(resolve, *type_id, fq);
        }

        let mut methods = Vec::new();
        for (fname, func) in &iface.functions {
            let cm_params: Vec<AttrArg> = func
                .params
                .iter()
                .map(|p| AttrArg::Str(p.name.clone()))
                .collect();
            let params: Vec<Param> = func
                .params
                .iter()
                .map(|p| {
                    let ty = self.map_type(resolve, p.ty, fq);
                    Param {
                        id: self.id(),
                        name: p.name.to_snake_case(),
                        name_span: syn(),
                        ty,
                        self_kind: SelfKind::None,
                        is_mut: false,
                        default: None,
                        span: syn(),
                    }
                })
                .collect();
            let return_type = func.result.map(|r| self.map_type(resolve, r, fq));
            let attrs = vec![
                self.cm_import_attr(fq, Some(fname)),
                Attribute {
                    name: "cm_params".to_string(),
                    args: cm_params,
                    cm_boundary: None,
                    span: syn(),
                },
            ];
            methods.push(InterfaceMethod {
                id: self.id(),
                name: fname.to_snake_case(),
                name_span: syn(),
                is_async: false,
                attrs,
                params,
                return_type,
                span: syn(),
            });
        }

        let name = iface
            .name
            .as_deref()
            .unwrap_or_default()
            .to_upper_camel_case();
        let attrs = vec![self.cm_import_attr(fq, None)];
        let id = self.id();
        self.items.push(Item::Interface(InterfaceDecl {
            id,
            name,
            name_span: syn(),
            visibility: Visibility::Public,
            attrs,
            methods,
            span: syn(),
        }));
    }

    fn emit_type_def(&mut self, resolve: &Resolve, type_id: TypeId, fq: &str) {
        let td = &resolve.types[type_id];
        let Some(wit_name) = td.name.as_deref() else {
            return; // anonymous structural type: rendered inline at use sites
        };
        let wado_name = wit_name.to_upper_camel_case();
        let cm = self.cm_import_attr(fq, Some(wit_name));

        match &td.kind {
            TypeDefKind::Record(rec) => {
                let fields = rec
                    .fields
                    .iter()
                    .map(|f| StructField {
                        id: self.id(),
                        name: f.name.to_snake_case(),
                        name_span: syn(),
                        visibility: Visibility::Public,
                        ty: self.map_type(resolve, f.ty, fq),
                        attrs: vec![self.cm_name_attr(&f.name)],
                        default: None,
                        span: syn(),
                    })
                    .collect();
                let id = self.id();
                self.items.push(Item::Struct(StructDecl {
                    id,
                    name: wado_name,
                    name_span: syn(),
                    visibility: Visibility::Public,
                    type_params: Vec::new(),
                    fields,
                    attrs: vec![cm],
                    span: syn(),
                }));
            }
            TypeDefKind::Enum(en) => {
                let cases = en
                    .cases
                    .iter()
                    .map(|c| EnumCase {
                        id: self.id(),
                        name: c.name.to_upper_camel_case(),
                        name_span: syn(),
                        attrs: vec![self.cm_name_attr(&c.name)],
                        span: syn(),
                    })
                    .collect();
                let id = self.id();
                self.items.push(Item::Enum(EnumDecl {
                    id,
                    name: wado_name,
                    name_span: syn(),
                    visibility: Visibility::Public,
                    type_params: Vec::new(),
                    cases,
                    attrs: vec![cm],
                    span: syn(),
                }));
            }
            TypeDefKind::Variant(var) => {
                let cases = var
                    .cases
                    .iter()
                    .map(|c| VariantCase {
                        id: self.id(),
                        name: c.name.to_upper_camel_case(),
                        name_span: syn(),
                        payload: c.ty.map(|t| self.map_type(resolve, t, fq)),
                        attrs: vec![self.cm_name_attr(&c.name)],
                        span: syn(),
                    })
                    .collect();
                let id = self.id();
                self.items.push(Item::Variant(VariantDecl {
                    id,
                    name: wado_name,
                    name_span: syn(),
                    visibility: Visibility::Public,
                    type_params: Vec::new(),
                    cases,
                    attrs: vec![cm],
                    span: syn(),
                }));
            }
            TypeDefKind::Flags(fl) => {
                let flags = fl
                    .flags
                    .iter()
                    .map(|m| FlagsVariant {
                        id: self.id(),
                        name: m.name.to_upper_camel_case(),
                        name_span: syn(),
                        attrs: vec![self.cm_name_attr(&m.name)],
                        span: syn(),
                    })
                    .collect();
                let id = self.id();
                self.items.push(Item::Flags(FlagsDecl {
                    id,
                    name: wado_name,
                    name_span: syn(),
                    visibility: Visibility::Public,
                    attributes: Some(vec![cm]),
                    flags,
                    span: syn(),
                }));
            }
            TypeDefKind::Type(inner) => {
                let ty = self.map_type(resolve, *inner, fq);
                let id = self.id();
                self.items.push(Item::Newtype(Newtype {
                    id,
                    name: wado_name,
                    name_span: syn(),
                    visibility: Visibility::Public,
                    type_params: Vec::new(),
                    ty,
                    attrs: vec![cm],
                    span: syn(),
                }));
            }
            other => {
                self.errors.push(format!(
                    "importing a component that exports the WIT type `{wit_name}` ({other:?}) is not \
                     yet supported"
                ));
            }
        }
    }

    /// Map a WIT type to its Wado AST type, reusing the [`CmShape`] structural
    /// classification shared with the producer.
    fn map_type(&mut self, resolve: &Resolve, ty: WitType, current_fq: &str) -> Type {
        match classify_wit(resolve, ty) {
            CmShape::Leaf => self.map_leaf(resolve, ty, current_fq),
            CmShape::Option(t) => {
                let inner = self.map_type(resolve, t, current_fq);
                self.generic("Option", vec![inner])
            }
            CmShape::List(t) => {
                let inner = self.map_type(resolve, t, current_fq);
                self.generic("List", vec![inner])
            }
            CmShape::Tuple(ts) => {
                let mut elems = Vec::with_capacity(ts.len());
                for t in ts {
                    elems.push(self.map_type(resolve, t, current_fq));
                }
                Type::Tuple(elems)
            }
            CmShape::Result { ok, err } => {
                let ok = match ok {
                    Some(t) => self.map_type(resolve, t, current_fq),
                    None => unit(),
                };
                let err = match err {
                    Some(t) => self.map_type(resolve, t, current_fq),
                    None => unit(),
                };
                self.generic("Result", vec![ok, err])
            }
            CmShape::Future(t) => {
                let args = match t {
                    Some(t) => vec![self.map_type(resolve, t, current_fq)],
                    None => Vec::new(),
                };
                self.generic("Future", args)
            }
            CmShape::Stream(t) => {
                let args = match t {
                    Some(t) => vec![self.map_type(resolve, t, current_fq)],
                    None => Vec::new(),
                };
                self.generic("Stream", args)
            }
        }
    }

    fn map_leaf(&mut self, resolve: &Resolve, ty: WitType, current_fq: &str) -> Type {
        // Inverse of `wit_emit::primitive_by_name`. Kept in lockstep with it.
        let prim = match ty {
            WitType::Bool => Some("bool"),
            WitType::U8 => Some("u8"),
            WitType::U16 => Some("u16"),
            WitType::U32 => Some("u32"),
            WitType::U64 => Some("u64"),
            WitType::S8 => Some("i8"),
            WitType::S16 => Some("i16"),
            WitType::S32 => Some("i32"),
            WitType::S64 => Some("i64"),
            WitType::F32 => Some("f32"),
            WitType::F64 => Some("f64"),
            WitType::Char => Some("char"),
            WitType::String => Some("String"),
            WitType::ErrorContext | WitType::Id(_) => None,
        };
        if let Some(name) = prim {
            return self.named(name, None);
        }
        let WitType::Id(id) = ty else {
            unreachable!("non-Id leaf handled above");
        };
        let td = &resolve.types[id];
        if let Some(name) = td.name.as_deref() {
            let src = type_source_fq(resolve, id, current_fq);
            self.named(&name.to_upper_camel_case(), Some(src))
        } else {
            self.errors.push(
                "encountered an unnameable WIT leaf type while importing a component".to_string(),
            );
            unit()
        }
    }

    fn named(&mut self, name: &str, source_interface: Option<String>) -> Type {
        Type::Named(NamedType {
            id: self.id(),
            name: name.to_string(),
            span: syn(),
            source_interface,
        })
    }

    fn generic(&mut self, name: &str, args: Vec<Type>) -> Type {
        Type::Generic(GenericType {
            id: self.id(),
            name: name.to_string(),
            args,
            span: syn(),
        })
    }
}

/// Classify a WIT type into its [`CmShape`]. Named type references and
/// primitives are leaves; anonymous structural defs unfold; a named-less alias
/// is transparent.
fn classify_wit(resolve: &Resolve, ty: WitType) -> CmShape<WitType> {
    let WitType::Id(id) = ty else {
        return CmShape::Leaf;
    };
    let td = &resolve.types[id];
    if td.name.is_some() {
        return CmShape::Leaf;
    }
    match &td.kind {
        TypeDefKind::Option(t) => CmShape::Option(*t),
        TypeDefKind::List(t) => CmShape::List(*t),
        TypeDefKind::Tuple(t) => CmShape::Tuple(t.types.clone()),
        TypeDefKind::Result(r) => CmShape::Result {
            ok: r.ok,
            err: r.err,
        },
        TypeDefKind::Future(t) => CmShape::Future(*t),
        TypeDefKind::Stream(t) => CmShape::Stream(*t),
        TypeDefKind::Type(inner) => classify_wit(resolve, *inner),
        _ => CmShape::Leaf,
    }
}

/// The FQ of the interface that owns `type_id`, falling back to `current_fq`
/// for types without an interface owner.
fn type_source_fq(resolve: &Resolve, type_id: TypeId, current_fq: &str) -> String {
    match resolve.types[type_id].owner {
        TypeOwner::Interface(id) => interface_fq(resolve, id),
        TypeOwner::World(_) | TypeOwner::None => current_fq.to_string(),
    }
}

/// `namespace:package/interface@version` for an interface.
fn interface_fq(resolve: &Resolve, iface_id: wit_parser::InterfaceId) -> String {
    let iface = &resolve.interfaces[iface_id];
    let name = iface.name.clone().unwrap_or_default();
    let Some(pkg_id) = iface.package else {
        return name;
    };
    let pkg = &resolve.packages[pkg_id].name;
    match &pkg.version {
        Some(v) => format!("{}:{}/{}@{}", pkg.namespace, pkg.name, name, v),
        None => format!("{}:{}/{}", pkg.namespace, pkg.name, name),
    }
}

fn unit() -> Type {
    Type::Tuple(Vec::new())
}

fn syn() -> Span {
    Span::new(0, 0, 1, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Item;

    fn decode_fixture() -> (Resolve, WorldId) {
        let bytes = std::fs::read("tests/fixtures/sub/cm-catalog.wasm").unwrap();
        match wit_component::decode(&bytes).unwrap() {
            wit_component::DecodedWasm::Component(r, w) => (r, w),
            wit_component::DecodedWasm::WitPackage(..) => panic!("expected component"),
        }
    }

    #[test]
    fn builds_catalog_bindings() {
        let (resolve, world) = decode_fixture();
        let b = build_bindings(&resolve, world).expect("build bindings");
        assert_eq!(
            b.interface_fqs,
            vec!["wado-lang:cm-catalog/cm-catalog@0.0.16"]
        );
        // cm-catalog is purely computational, so its only host-leaf imports are
        // the ambient panic path (`wasi:cli/stderr` + the shared `wasi:cli/types`
        // error code) every Wado component declares. Effect reconstruction
        // subtracts these ambient interfaces, so the effect set is still empty;
        // the raw collection here keeps them faithfully for the import plan.
        assert_eq!(
            b.host_leaf_imports,
            vec![
                "wasi:cli/types@0.3.0".to_string(),
                "wasi:cli/stderr@0.3.0".to_string(),
            ]
        );

        let mut iface = None;
        let mut type_names = Vec::new();
        for item in &b.module.items {
            match item {
                Item::Interface(d) => iface = Some(d),
                Item::Struct(d) => type_names.push(d.name.clone()),
                Item::Enum(d) => type_names.push(d.name.clone()),
                Item::Variant(d) => type_names.push(d.name.clone()),
                Item::Flags(d) => type_names.push(d.name.clone()),
                Item::Newtype(d) => type_names.push(d.name.clone()),
                _ => {}
            }
        }
        let iface = iface.expect("interface present");
        assert_eq!(iface.name, "CmCatalog");

        // A primitive identity and a named-type identity, with cm metadata.
        let id_u32 = iface.methods.iter().find(|m| m.name == "id_u32").unwrap();
        let cm = id_u32.attrs[0].as_cm_import().unwrap();
        assert_eq!(cm.interface_path(), "wado-lang:cm-catalog/cm-catalog@0.0.16");
        assert_eq!(cm.function.as_deref(), Some("id-u32"));

        let id_record = iface
            .methods
            .iter()
            .find(|m| m.name == "id_record")
            .unwrap();
        match &id_record.params[0].ty {
            Type::Named(n) => {
                assert_eq!(n.name, "Point");
                assert_eq!(
                    n.source_interface.as_deref(),
                    Some("wado-lang:cm-catalog/cm-catalog@0.0.16")
                );
            }
            other => panic!("expected Named Point, got {other:?}"),
        }

        // Records/enums/variants/flags and the `meters = f64` newtype are all
        // preserved as named types at the CM boundary, so the importer sees the
        // newtype rather than an inlined `f64` (issue #1456).
        for t in ["Point", "Color", "Shape", "Perms", "Mixed", "Meters"] {
            assert!(type_names.contains(&t.to_string()), "missing type {t}");
        }
        let id_newtype = iface
            .methods
            .iter()
            .find(|m| m.name == "id_newtype")
            .unwrap();
        match &id_newtype.params[0].ty {
            Type::Named(n) => {
                assert_eq!(n.name, "Meters");
                assert_eq!(
                    n.source_interface.as_deref(),
                    Some("wado-lang:cm-catalog/cm-catalog@0.0.16")
                );
            }
            other => panic!("expected Named Meters, got {other:?}"),
        }
    }
}
