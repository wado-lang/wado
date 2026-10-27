//! Build a Wado AST module from a decoded CM component's WIT type.
//!
//! The consumer side of WIT interoperability: turns the `wit_parser::Resolve`
//! from `wit_component::decode` into an `ast::Module` of `#[cm(...)]`-annotated
//! interfaces, functions, and types — the shape `wado-from-idl` emits for the
//! stdlib, but built directly as AST. The normal frontend then handles it.
//!
//! Type mapping reuses [`crate::wit_emit`]'s `CmShape` so producer and consumer
//! cannot drift on how `option`/`list`/`tuple`/`result` are structured.

use crate::ast::{
    AstId, AstIdSpace, AttrArg, Attribute, CmBoundary, CmImport, EffectsSource, EnumCase, EnumDecl,
    FlagsDecl, FlagsVariant, Function, GenericType, InnerAttribute, InterfaceDecl, Item, Module,
    NamedType, Newtype, Param, SelfKind, StrLit, StructDecl, StructField, Type, UseDecl, UseItem,
    UseNameSpelling, VariantCase, VariantDecl, Visibility, cm_import_of,
};
use crate::attribute::{CM, CM_EXPORT, CM_HOST_IMPORTS, CM_PARAMS};
use crate::component_model::SourceInterfaceBatch;
use crate::hashmap;
use crate::module_source::ModuleSource;
use crate::name::{
    cm_dependency_interface, interface_basename, mangle_local_method, unnamed_interface_name,
    wado_identifier,
};
use crate::token::Span;
use crate::wit_emit::CmShape;
use heck::{ToSnakeCase, ToUpperCamelCase};
use wit_parser::{
    Handle, Resolve, Type as WitType, TypeDefKind, TypeId, TypeOwner, WorldId, WorldItem,
};

/// The Wado bindings synthesized from one decoded imported component.
pub struct ComponentBindings {
    /// The AST module declaring the component's exported interfaces and types.
    pub module: Module,
    /// Every interface the component exports.
    pub interfaces: Vec<ExportedInterface>,
    /// FQ names of the interfaces the component itself imports — its host-leaf
    /// capabilities (WASI, etc.). Effect reconstruction maps these onto the
    /// consumer's effects; a purely-computational component imports none.
    pub host_leaf_imports: Vec<String>,
    /// The CM interface each named-type reference in `module` resolves to,
    /// keyed by the reference's own site. Merged into the CM registry by the
    /// loader; the syntax nodes carry no interface of their own.
    pub source_interfaces: SourceInterfaceBatch,
}

/// An interface an imported component exports.
#[derive(Debug, Clone)]
pub struct ExportedInterface {
    /// Its name in the component.
    pub export: String,
    /// The interface the program imports it as, which composition wires to
    /// `export` ([`cm_dependency_interface`]).
    pub import: String,
}

/// The host-leaf import FQs a component-binding module carries in its
/// [`CM_HOST_IMPORTS`] inner attribute. Empty where it carries none.
pub fn module_host_leaf_imports(module: &Module) -> Vec<String> {
    module
        .inner_attributes
        .iter()
        .find(|a| a.name == CM_HOST_IMPORTS)
        .map(|a| {
            a.args
                .iter()
                .filter_map(|arg| {
                    if let AttrArg::Str(s) = arg {
                        Some(s.value.clone())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The interfaces a component-binding module's dependency exports, each with
/// its `#[cm]` import. A reconstructed import interface (a guest effect the
/// consumer provides) sits in the same module but is not among them, so it is
/// excluded via the module's host-leaf import list. Empty for a core-wasm
/// asset, whose decls carry `#[canonical(...)]`.
pub fn exported_interfaces(module: &Module) -> impl Iterator<Item = (&InterfaceDecl, &CmImport)> {
    interfaces_on(module, Side::Export)
}

/// The interfaces a component-binding module's dependency imports and its
/// consumer provides, each with its `#[cm]` import.
pub fn imported_interfaces(module: &Module) -> impl Iterator<Item = (&InterfaceDecl, &CmImport)> {
    interfaces_on(module, Side::Import)
}

/// Which side of a component an interface its bindings declare sits on.
#[derive(Clone, Copy, PartialEq)]
enum Side {
    Export,
    Import,
}

fn interfaces_on(module: &Module, side: Side) -> impl Iterator<Item = (&InterfaceDecl, &CmImport)> {
    let imports: hashmap::IndexSet<String> = module_host_leaf_imports(module).into_iter().collect();
    module.items.iter().filter_map(move |item| match item {
        Item::Interface(decl) => cm_import_of(&decl.attrs)
            .filter(|cm| imports.contains(&cm.interface_path()) == (side == Side::Import))
            .map(|cm| (decl, cm)),
        _ => None,
    })
}

/// Build a Wado AST module from a component's decoded `Resolve` + target world.
///
/// # Errors
/// A message naming each shape the component exports that has no import
/// mapping yet: a resource, a world-level type, or `error-context`.
pub fn build_bindings(
    resolve: &Resolve,
    world: WorldId,
    component: &ModuleSource,
) -> Result<ComponentBindings, String> {
    let namespace = component
        .wasm_canonical_namespace()
        .expect("a component is a wasm asset");
    let exports: hashmap::IndexMap<String, String> = resolve.worlds[world]
        .exports
        .values()
        .filter_map(|item| match item {
            WorldItem::Interface { id, .. } => Some(interface_fq(resolve, *id)),
            WorldItem::Function(_) | WorldItem::Type { .. } => None,
        })
        .map(|fq| {
            let cm = CmImport::parse(&fq).expect("interface FQ is well-formed");
            let import = cm_dependency_interface(&cm, &namespace).interface_path();
            (fq, import)
        })
        .collect();
    // A non-WASI import is a guest effect the consumer must provide. WASI
    // leaves resolve to their auto-loaded stdlib effect and need no synthesis.
    let guest_imports: Vec<wit_parser::InterfaceId> = resolve.worlds[world]
        .imports
        .values()
        .filter_map(|item| match item {
            WorldItem::Interface { id, .. } if !interface_fq(resolve, *id).starts_with("wasi:") => {
                Some(*id)
            }
            WorldItem::Interface { .. } | WorldItem::Function(_) | WorldItem::Type { .. } => None,
        })
        .collect();
    let mut b = Builder::new(
        interface_names(resolve, world, &exports, &guest_imports),
        exports,
    );

    for (_, item) in &resolve.worlds[world].exports {
        match item {
            WorldItem::Interface { id, .. } => {
                let fq = interface_fq(resolve, *id);
                b.emit_interface(resolve, *id, &fq);
            }
            WorldItem::Function(f) => b.emit_world_function(resolve, f),
            WorldItem::Type { .. } => {
                return Err(
                    "importing a component that exports a world-level type is not yet supported"
                        .to_string(),
                );
            }
        }
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

    // A guest effect becomes an impl-able `#[cm]` interface, so effect
    // reconstruction resolves the requirement and a `with h do` handler can
    // satisfy it.
    for id in guest_imports {
        let fq = interface_fq(resolve, id);
        b.emit_interface(resolve, id, &fq);
    }

    if !b.errors.is_empty() {
        // One unsupported shape reaches the collector once per signature
        // position it occupies; the reader needs it once.
        let unique: hashmap::IndexSet<String> = b.errors.into_iter().collect();
        return Err(unique.into_iter().collect::<Vec<_>>().join("; "));
    }

    // Carry the host-leaf set on the synthesized module so the registration
    // path (`fold_component_interfaces`) recovers it from the AST alone.
    let inner_attributes = if host_leaf_imports.is_empty() {
        Vec::new()
    } else {
        vec![InnerAttribute {
            name: CM_HOST_IMPORTS.to_string(),
            args: host_leaf_imports
                .iter()
                .map(|import| AttrArg::Str(StrLit::new(import.as_str())))
                .collect(),
            span: syn(),
        }]
    };

    if b.uses_tree_map {
        let tree_map_use = b.use_item("core:collections", "TreeMap");
        b.items.insert(0, tree_map_use);
    }
    if b.uses_list_view {
        let list_view_use = b.use_item("core:linear", "ListView");
        b.items.insert(0, list_view_use);
    }
    let source_interfaces = b.source_interfaces;
    let module = Module::with_metadata(
        b.items,
        inner_attributes,
        None,
        None,
        hashmap::IndexSet::default(),
        b.space,
        b.count,
        false,
        // Synthesized from WIT, never highlighted: no source text to key on.
        Vec::new(),
    );
    let interfaces = b
        .exports
        .into_iter()
        .map(|(export, import)| ExportedInterface { export, import })
        .collect();
    Ok(ComponentBindings {
        module,
        interfaces,
        host_leaf_imports,
        source_interfaces,
    })
}

/// What a component's bindings declare each interface, and each of its types,
/// under.
struct InterfaceNames {
    /// Each interface the bindings declare -> its name.
    interfaces: hashmap::IndexMap<String, String>,
    /// The default interfaces the component exports, whose types are items of
    /// the module. Any other interface's types are named through it.
    defaults: hashmap::IndexSet<String>,
}

/// Name each interface the bindings declare by its basename, or, where
/// another interface shares the basename or a higher rank takes it (a world
/// function, a default interface's type), by its interface name, which only a
/// full name reaches.
fn interface_names(
    resolve: &Resolve,
    world: WorldId,
    exports: &hashmap::IndexMap<String, String>,
    guest_imports: &[wit_parser::InterfaceId],
) -> InterfaceNames {
    let parse = |fq: &str| CmImport::parse(fq).expect("interface FQ is well-formed");
    let exported: Vec<CmImport> = exports.keys().map(|fq| parse(fq)).collect();
    let defaults: hashmap::IndexSet<String> = exported
        .iter()
        .filter(|cm| cm.is_package_default())
        .map(CmImport::interface_path)
        .collect();
    let declared: Vec<CmImport> = exported
        .into_iter()
        .chain(
            guest_imports
                .iter()
                .map(|id| parse(&interface_fq(resolve, *id))),
        )
        .collect();
    let mut taken: hashmap::IndexSet<String> = hashmap::IndexSet::default();
    for item in resolve.worlds[world].exports.values() {
        match item {
            WorldItem::Function(func) => {
                taken.insert(wado_identifier(&func.name));
            }
            WorldItem::Interface { id, .. } if defaults.contains(&interface_fq(resolve, *id)) => {
                taken.extend(
                    resolve.interfaces[*id]
                        .types
                        .keys()
                        .map(|name| name.to_upper_camel_case()),
                );
            }
            WorldItem::Interface { .. } | WorldItem::Type { .. } => {}
        }
    }
    let mut sharing: hashmap::IndexMap<String, usize> = hashmap::IndexMap::default();
    for cm in &declared {
        *sharing.entry(interface_basename(cm)).or_default() += 1;
    }
    let interfaces = declared
        .iter()
        .map(|cm| {
            let basename = interface_basename(cm);
            let name = if sharing[&basename] > 1 || taken.contains(&basename) {
                unnamed_interface_name(cm)
            } else {
                basename
            };
            (cm.interface_path(), name)
        })
        .collect();
    InterfaceNames {
        interfaces,
        defaults,
    }
}

/// Allocates dense, module-local `AstId`s while building the synthesized module.
struct Builder {
    space: AstIdSpace,
    count: u32,
    items: Vec<Item>,
    errors: Vec<String>,
    source_interfaces: SourceInterfaceBatch,
    uses_tree_map: bool,
    uses_list_view: bool,
    names: InterfaceNames,
    /// Each interface the component exports -> the one the program imports it
    /// as ([`cm_dependency_interface`]).
    exports: hashmap::IndexMap<String, String>,
}

impl Builder {
    fn new(names: InterfaceNames, exports: hashmap::IndexMap<String, String>) -> Self {
        Self {
            space: AstIdSpace::next(),
            count: 0,
            items: Vec::new(),
            errors: Vec::new(),
            source_interfaces: SourceInterfaceBatch::default(),
            uses_tree_map: false,
            uses_list_view: false,
            names,
            exports,
        }
    }

    /// The name the bindings declare the WIT type `wit_name` of the interface
    /// `fq` under: an item of the module for a default interface's type, a
    /// member of its interface otherwise. A type of an interface the bindings
    /// do not declare is the standard library's, by its own name.
    fn type_name(&self, fq: &str, wit_name: &str) -> String {
        let name = wit_name.to_upper_camel_case();
        match self.names.interfaces.get(fq) {
            Some(interface) if !self.names.defaults.contains(fq) => {
                mangle_local_method(interface, &name)
            }
            Some(_) | None => name,
        }
    }

    /// `use { name } from "source";`, for a binding type or trait the prelude
    /// does not supply.
    fn use_item(&mut self, source: &str, name: &str) -> Item {
        Item::Use(UseDecl {
            id: self.id(),
            attrs: Vec::new(),
            visibility: Visibility::Private,
            is_export: false,
            source: StrLit::new(source),
            source_span: syn(),
            source_id: self.id(),
            items: vec![UseItem::Simple {
                id: self.id(),
                name: name.to_string(),
                spelling: UseNameSpelling::Ident,
                name_span: syn(),
                alias: None,
                local_span: syn(),
            }],
            items_span: Some(syn()),
            attributes: None,
            span: syn(),
        })
    }

    fn id(&mut self) -> AstId {
        let id = AstId::new(self.space, self.count);
        self.count += 1;
        id
    }

    /// The interface the program imports the component's interface `fq` as:
    /// one it imports itself keeps its name.
    fn import_fq(&self, fq: &str) -> String {
        self.exports
            .get(fq)
            .cloned()
            .unwrap_or_else(|| fq.to_string())
    }

    /// `#[cm("ns:pkg/iface@ver[#frag]")]` as an interface-import boundary, for
    /// the interface `fq` the program imports.
    fn cm_import_attr(&self, fq: &str, frag: Option<&str>) -> Attribute {
        let mut cm = CmImport::parse(&self.import_fq(fq)).expect("interface FQ is well-formed");
        cm.function = frag.map(str::to_string);
        Attribute {
            name: CM.to_string(),
            args: Vec::new(),
            cm_boundary: Some(CmBoundary::Import(cm)),
            span: syn(),
        }
    }

    /// `#[cm]` as a world-level function-import boundary (Phase 9).
    fn cm_world_import_attr(&self, func_name: &str) -> Attribute {
        Attribute {
            name: CM.to_string(),
            args: Vec::new(),
            cm_boundary: Some(CmBoundary::WorldImport(func_name.to_string())),
            span: syn(),
        }
    }

    /// `#[cm("kebab-name")]` as a bare CM-name boundary (fields, cases, flags).
    fn cm_name_attr(&self, name: &str) -> Attribute {
        Attribute {
            name: CM.to_string(),
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

        let iface_name = self.names.interfaces[fq].clone();

        let mut methods = Vec::new();
        for (fname, func) in &iface.functions {
            let signature = self.signature(resolve, func, fq);
            let is_async = is_async_func(func);
            let return_type = self.func_return_type(resolve, func, fq);
            let attrs = vec![self.cm_import_attr(fq, Some(fname)), signature.cm_params];
            methods.push(Function {
                id: self.id(),
                name: wado_identifier(fname),
                name_span: syn(),
                visibility: Visibility::Private,
                is_export: false,
                export_targets: Vec::new(),
                is_async,
                type_params: Vec::new(),
                attrs,
                params: signature.params,
                params_span: syn(),
                return_type,
                effects: Vec::new(),
                effects_source: EffectsSource::Written,
                holds_contracts_of: None,
                body: None,
                span: syn(),
            });
        }

        let mut attrs = vec![self.cm_import_attr(fq, None)];
        if self.exports.contains_key(fq) {
            attrs.push(Attribute {
                name: CM_EXPORT.to_string(),
                args: vec![AttrArg::Str(StrLit::new(fq))],
                cm_boundary: None,
                span: syn(),
            });
        }
        let id = self.id();
        self.items.push(Item::Interface(InterfaceDecl {
            id,
            name: iface_name,
            name_span: syn(),
            visibility: Visibility::Public,
            attrs,
            methods,
            span: syn(),
        }));
    }

    /// `func`'s parameters, with the `#[cm_params]` naming them in WIT.
    fn signature(&mut self, resolve: &Resolve, func: &wit_parser::Function, fq: &str) -> Signature {
        let cm_params = Attribute {
            name: CM_PARAMS.to_string(),
            args: func
                .params
                .iter()
                .map(|p| AttrArg::Str(StrLit::new(p.name.as_str())))
                .collect(),
            cm_boundary: None,
            span: syn(),
        };
        let mut params = Vec::with_capacity(func.params.len());
        for p in &func.params {
            let ty = match lent_list_elem(resolve, func, p.ty) {
                Some(elem) => {
                    let elem = self.map_leaf(resolve, elem, fq);
                    self.uses_list_view = true;
                    Type::Reference(Box::new(self.generic("ListView", vec![elem])))
                }
                None => self.map_type(resolve, p.ty, fq),
            };
            params.push(Param {
                id: self.id(),
                attrs: Vec::new(),
                name: wado_identifier(&p.name),
                name_span: syn(),
                ty,
                self_kind: SelfKind::None,
                is_mut: false,
                default: None,
                span: syn(),
            });
        }
        Signature { params, cm_params }
    }

    /// A WIT result, wrapped in `AsyncCall<T>` for an `async func` — the shape
    /// the consumer drives the subtask through.
    fn func_return_type(
        &mut self,
        resolve: &Resolve,
        func: &wit_parser::Function,
        fq: &str,
    ) -> Option<Type> {
        let result = func.result.map(|r| self.map_type(resolve, r, fq));
        if is_async_func(func) {
            let result = result.unwrap_or_else(|| self.unit());
            return Some(self.generic("AsyncCall", vec![result]));
        }
        result
    }

    /// Emit a bodyless free `Item::Function` for a world-level function export
    /// (Phase 9); the consumer calls it as a free function and codegen
    /// synthesizes the CM import trampoline from the `#[cm]` boundary.
    fn emit_world_function(&mut self, resolve: &Resolve, func: &wit_parser::Function) {
        let is_async = is_async_func(func);
        let signature = self.signature(resolve, func, "");
        let return_type = self.func_return_type(resolve, func, "");
        let attrs = vec![self.cm_world_import_attr(&func.name), signature.cm_params];
        let id = self.id();
        self.items.push(Item::Function(Function {
            id,
            name: wado_identifier(&func.name),
            name_span: syn(),
            visibility: Visibility::Public,
            is_export: false,
            export_targets: Vec::new(),
            is_async,
            type_params: Vec::new(),
            attrs,
            params: signature.params,
            params_span: syn(),
            return_type,
            effects: Vec::new(),
            effects_source: EffectsSource::Written,
            holds_contracts_of: None,
            body: None,
            span: syn(),
        }));
    }

    fn emit_type_def(&mut self, resolve: &Resolve, type_id: TypeId, fq: &str) {
        let td = &resolve.types[type_id];
        let Some(wit_name) = td.name.as_deref() else {
            return; // anonymous structural type: rendered inline at use sites
        };
        if used_origin(resolve, type_id) != type_id {
            return; // a WIT `use`: a site naming it names the other interface's type
        }
        let wado_name = self.type_name(fq, wit_name);
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
                    is_export: false,
                    is_affine: wit_holds_affine(resolve, WitType::Id(type_id)),
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
                    is_export: false,
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
                    is_export: false,
                    is_affine: wit_holds_affine(resolve, WitType::Id(type_id)),
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
                    is_export: false,
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
                    is_export: false,
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
            CmShape::Map(k, v) => {
                let key = self.map_type(resolve, k, current_fq);
                let value = self.map_type(resolve, v, current_fq);
                self.uses_tree_map = true;
                self.generic("TreeMap", vec![key, value])
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
                    None => self.unit(),
                };
                let err = match err {
                    Some(t) => self.map_type(resolve, t, current_fq),
                    None => self.unit(),
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
            WitType::ErrorContext => {
                self.errors.push(
                    "importing a component whose signature carries `error-context` is not yet \
                     supported"
                        .to_string(),
                );
                return self.unit();
            }
            WitType::Id(_) => None,
        };
        if let Some(name) = prim {
            return self.named(name, None);
        }
        let WitType::Id(id) = ty else {
            unreachable!("non-Id leaf handled above");
        };
        let id = used_origin(resolve, id);
        let td = &resolve.types[id];
        if let Some(name) = td.name.as_deref() {
            let owner = type_source_fq(resolve, id, current_fq);
            let declared = self.type_name(&owner, name);
            let src = self.import_fq(&owner);
            self.named(&declared, Some(src))
        } else {
            self.errors.push(
                "encountered an unnameable WIT leaf type while importing a component".to_string(),
            );
            self.unit()
        }
    }

    fn named(&mut self, name: &str, source_interface: Option<String>) -> Type {
        let id = self.id();
        if let Some(source) = source_interface {
            self.source_interfaces.insert(id, source);
        }
        Type::Named(NamedType {
            id,
            name: name.to_string(),
            span: syn(),
        })
    }

    fn unit(&mut self) -> Type {
        Type::unit(self.id(), syn())
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

/// A function's parameters as the bindings declare them.
struct Signature {
    params: Vec<Param>,
    /// `#[cm_params]`, the WIT name of each parameter.
    cm_params: Attribute,
}

/// The `T` an import `func` takes its parameter typed `ty` as a `&ListView<T>`
/// at, so a `LinearArray` lends its own block (WEP: Linear Arrays): a
/// `list<T>` of numbers, on a synchronous function. An async one has not read
/// its arguments by the time it returns, so it keeps the `List`.
pub fn lent_list_elem(
    resolve: &Resolve,
    func: &wit_parser::Function,
    ty: WitType,
) -> Option<WitType> {
    (!is_async_func(func))
        .then(|| list_view_elem(resolve, ty))
        .flatten()
}

/// The `T` of `ty` when it is a `list<T>` of a Component Model number type,
/// which a `ListView<T>` lays out as the Canonical ABI does.
fn list_view_elem(resolve: &Resolve, ty: WitType) -> Option<WitType> {
    let CmShape::List(elem) = classify_wit(resolve, ty) else {
        return None;
    };
    matches!(
        elem,
        WitType::U8
            | WitType::U16
            | WitType::U32
            | WitType::U64
            | WitType::S8
            | WitType::S16
            | WitType::S32
            | WitType::S64
            | WitType::F32
            | WitType::F64
    )
    .then_some(elem)
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
        TypeDefKind::Map(k, v) => CmShape::Map(*k, *v),
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

/// Whether a value of WIT type `ty` holds an `own` handle, a stream or a future
/// at any depth, which makes the Wado type it maps to affine. WIT admits no
/// recursive type, so the walk ends.
pub fn wit_holds_affine(resolve: &Resolve, ty: WitType) -> bool {
    let WitType::Id(id) = ty else {
        return false;
    };
    let holds = |t: &WitType| wit_holds_affine(resolve, *t);
    match &resolve.types[id].kind {
        TypeDefKind::Handle(Handle::Own(_))
        | TypeDefKind::Resource
        | TypeDefKind::Future(_)
        | TypeDefKind::Stream(_) => true,
        TypeDefKind::Handle(Handle::Borrow(_))
        | TypeDefKind::Flags(_)
        | TypeDefKind::Enum(_)
        | TypeDefKind::Unknown => false,
        TypeDefKind::Record(r) => r.fields.iter().any(|f| holds(&f.ty)),
        TypeDefKind::Tuple(t) => t.types.iter().any(holds),
        TypeDefKind::Variant(v) => v.cases.iter().any(|c| c.ty.as_ref().is_some_and(holds)),
        TypeDefKind::Option(t)
        | TypeDefKind::List(t)
        | TypeDefKind::FixedLengthList(t, _)
        | TypeDefKind::Type(t) => holds(t),
        TypeDefKind::Map(k, v) => holds(k) || holds(v),
        TypeDefKind::Result(r) => {
            r.ok.as_ref().is_some_and(holds) || r.err.as_ref().is_some_and(holds)
        }
    }
}

/// Whether a WIT function is declared `async func`, which the consumer calls
/// through an `AsyncCall<T>` like any other CM async import.
fn is_async_func(func: &wit_parser::Function) -> bool {
    use wit_parser::FunctionKind;
    match func.kind {
        FunctionKind::AsyncFreestanding
        | FunctionKind::AsyncMethod(_)
        | FunctionKind::AsyncStatic(_) => true,
        FunctionKind::Freestanding
        | FunctionKind::Getter
        | FunctionKind::Setter
        | FunctionKind::Method(_)
        | FunctionKind::MethodGetter(_)
        | FunctionKind::MethodSetter(_)
        | FunctionKind::Static(_)
        | FunctionKind::StaticGetter(_)
        | FunctionKind::StaticSetter(_)
        | FunctionKind::Constructor(_) => false,
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

/// The type a WIT `use` of `type_id` reaches, through any chain of them, or
/// `type_id` itself. A `use` is a named alias of a type another interface owns;
/// an alias within one interface is a `type` declaration of its own.
fn used_origin(resolve: &Resolve, type_id: TypeId) -> TypeId {
    let td = &resolve.types[type_id];
    if let TypeDefKind::Type(WitType::Id(inner)) = td.kind
        && resolve.types[inner].name.is_some()
        && resolve.types[inner].owner != td.owner
    {
        return used_origin(resolve, inner);
    }
    type_id
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

fn syn() -> Span {
    Span::new(0, 0, 1, 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Item, cm_import_of, cm_interface_name_of};
    use crate::component_model::CmInterfaceRegistry;
    use crate::module_source::{ModuleSourceInterner, WasmAssetKind};

    fn decode_fixture() -> (Resolve, WorldId) {
        let bytes = std::fs::read("tests/fixtures/sub/cm-catalog.wasm").unwrap();
        match wit_component::decode(&bytes).unwrap() {
            wit_component::DecodedWasm::Component(r, w) => (r, w),
            wit_component::DecodedWasm::WitPackage(..) => panic!("expected component"),
        }
    }

    /// The fixture's own interface FQ, read from the component. The binary is
    /// committed, so a version taken from anywhere else drifts from it.
    fn catalog_fq(resolve: &Resolve) -> String {
        let name = resolve
            .packages
            .iter()
            .map(|(_, package)| &package.name)
            .find(|name| name.name == "cm-catalog")
            .expect("the fixture declares the cm-catalog package");
        let version = name.version.as_ref().expect("the fixture is versioned");
        format!("{}:{}/cm-catalog@{version}", name.namespace, name.name)
    }

    fn component() -> ModuleSource {
        ModuleSourceInterner::new().wasm("cm-catalog.wasm", WasmAssetKind::Wasm)
    }

    #[test]
    fn builds_catalog_bindings() {
        let (resolve, world) = decode_fixture();
        let component = component();
        let b = build_bindings(&resolve, world, &component).expect("build bindings");
        let catalog_fq = catalog_fq(&resolve);
        // The program imports the interface under a name of this component's.
        let catalog_import = cm_dependency_interface(
            &CmImport::parse(&catalog_fq).unwrap(),
            &component.wasm_canonical_namespace().unwrap(),
        )
        .interface_path();
        let registry = CmInterfaceRegistry::new();
        registry.extend_source_interfaces(b.source_interfaces.clone());
        let [exported] = b.interfaces.as_slice() else {
            panic!("cm-catalog exports one interface");
        };
        assert_eq!(exported.export, catalog_fq);
        assert_eq!(exported.import, catalog_import);
        // cm-catalog performs no I/O, so the ambient panic path imports nothing
        // (`log_stderr` rides an existing stderr import or drops its message —
        // see `AmbientStdio`). Its one host-leaf import is
        // `wasi:cli/types`, the shared error-code the future/stream results of
        // its async exports carry — a genuine capability of the async surface,
        // not an ambient effect.
        assert_eq!(
            b.host_leaf_imports,
            vec!["wasi:cli/types@0.3.0".to_string()]
        );

        let mut iface = None;
        let mut type_names = Vec::new();
        for item in &b.module.items {
            if let Item::Interface(d) = item {
                iface = Some(d);
            } else if let Some(decl) = item.type_decl() {
                type_names.push(decl.name.to_string());
            }
        }
        let iface = iface.expect("interface present");
        assert_eq!(iface.name, "CmCatalog");
        assert_eq!(
            cm_interface_name_of(&iface.attrs).unwrap().interface_path(),
            catalog_fq
        );

        // A primitive identity and a named-type identity, with cm metadata.
        let id_u32 = iface.methods.iter().find(|m| m.name == "id_u32").unwrap();
        let cm = cm_import_of(&id_u32.attrs).unwrap();
        assert_eq!(cm.interface_path(), catalog_import);
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
                    registry.source_interface(n).as_deref(),
                    Some(catalog_import.as_str())
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
                    registry.source_interface(n).as_deref(),
                    Some(catalog_import.as_str())
                );
            }
            other => panic!("expected Named Meters, got {other:?}"),
        }
    }

    #[test]
    fn a_keyword_named_function_or_param_is_escaped() {
        let mut resolve = Resolve::new();
        let package = resolve
            .push_str(
                "k.wit",
                "package t:k;
                 interface ops { match: func(resume: u32) -> u32; }
                 world w { export ops; export resume: func(match: u32); }",
            )
            .unwrap();
        let world = resolve.select_world(&[package], Some("w")).unwrap();
        let b = build_bindings(&resolve, world, &component()).expect("build bindings");
        let mut names = Vec::new();
        for item in &b.module.items {
            let functions = match item {
                Item::Interface(d) => d.methods.iter().collect(),
                Item::Function(f) => vec![f],
                _ => Vec::new(),
            };
            for f in functions {
                names.push(f.name.clone());
                names.extend(f.params.iter().map(|p| p.name.clone()));
            }
        }
        names.sort();
        assert_eq!(names, ["match_", "match_", "resume_", "resume_"]);
    }
}
