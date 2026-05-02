//! Intermediate representation for Wado code generation

/// A cross-interface type import
#[derive(Debug, Clone)]
pub struct WadoImport {
    /// Type names to import (e.g., `["Instant"]`)
    pub type_names: Vec<String>,
    /// Source path (e.g., "wasi:clocks/system-clock")
    pub from_path: String,
}

/// A complete Wado module to be generated
#[derive(Debug, Clone)]
pub struct WadoModule {
    pub package_name: String,
    pub package_version: String,
    pub source_files: Vec<String>,
    /// Cross-interface `use` imports (e.g., `use { Instant } from "wasi:clocks/system-clock"`)
    pub imports: Vec<WadoImport>,
    pub types: Vec<WadoTypeDef>,
    pub resources: Vec<WadoResource>,
    pub interfaces: Vec<WadoInterface>,
    pub standalone_functions: Vec<WadoFunction>,
    pub worlds: Vec<WadoWorld>,
}

impl WadoModule {
    #[must_use]
    pub fn new(package_name: String, package_version: String) -> Self {
        Self {
            package_name,
            package_version,
            source_files: Vec::new(),
            imports: Vec::new(),
            types: Vec::new(),
            resources: Vec::new(),
            interfaces: Vec::new(),
            standalone_functions: Vec::new(),
            worlds: Vec::new(),
        }
    }

    /// Collect all public names exported by this module (types, resources, effects).
    ///
    /// Skips self-referential newtypes (e.g., `type Instant = Instant`) which arise
    /// from WIT's cross-interface `use` statements and are not defined in this interface.
    #[must_use]
    pub fn public_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        for ty in &self.types {
            match ty {
                // Skip self-referential newtypes: they're cross-interface aliases, not definitions
                WadoTypeDef::Newtype(n) => {
                    if let WadoType::Named(target) = &n.target
                        && target == &n.name
                    {
                        continue;
                    }
                    names.push(n.name.clone());
                }
                WadoTypeDef::Enum(e) => names.push(e.name.clone()),
                WadoTypeDef::Flags(f) => names.push(f.name.clone()),
                WadoTypeDef::Struct(s) => names.push(s.name.clone()),
                WadoTypeDef::Variant(v) => names.push(v.name.clone()),
            }
        }
        for r in &self.resources {
            names.push(r.name.clone());
        }
        for e in &self.interfaces {
            names.push(e.name.clone());
        }
        names
    }
}

/// Type definition in Wado
#[derive(Debug, Clone)]
pub enum WadoTypeDef {
    Enum(WadoEnum),
    Flags(WadoFlags),
    Struct(WadoStruct),
    Variant(WadoVariant),
    Newtype(WadoNewtype),
}

#[derive(Debug, Clone)]
pub struct WadoEnum {
    pub name: String,
    pub doc_comment: Option<String>,
    pub cm_attr: Option<String>,
    pub variants: Vec<WadoEnumVariant>,
}

#[derive(Debug, Clone)]
pub struct WadoEnumVariant {
    pub name: String,
    pub doc_comment: Option<String>,
    /// Original WIT kebab-case name for the `#[cm("...")]` attribute
    pub cm_attr: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WadoFlags {
    pub name: String,
    pub doc_comment: Option<String>,
    pub cm_attr: Option<String>,
    pub flags: Vec<WadoFlagMember>,
}

#[derive(Debug, Clone)]
pub struct WadoFlagMember {
    pub name: String,
    pub doc_comment: Option<String>,
    /// Original WIT kebab-case name for the `#[cm("...")]` attribute
    pub cm_attr: String,
}

#[derive(Debug, Clone)]
pub struct WadoStruct {
    pub name: String,
    pub doc_comment: Option<String>,
    pub cm_attr: Option<String>,
    pub fields: Vec<WadoField>,
}

#[derive(Debug, Clone)]
pub struct WadoField {
    pub name: String,
    pub ty: WadoType,
    pub doc_comment: Option<String>,
    /// Original WIT kebab-case name for the `#[cm("...")]` attribute
    pub cm_attr: String,
}

#[derive(Debug, Clone)]
pub struct WadoVariant {
    pub name: String,
    pub doc_comment: Option<String>,
    pub cm_attr: Option<String>,
    pub cases: Vec<WadoVariantCase>,
}

#[derive(Debug, Clone)]
pub struct WadoVariantCase {
    pub name: String,
    pub payload: Option<WadoType>,
    pub doc_comment: Option<String>,
    /// Original WIT kebab-case name for the `#[cm("...")]` attribute
    pub cm_attr: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WadoNewtype {
    pub name: String,
    pub cm_attr: Option<String>,
    pub target: WadoType,
}

#[derive(Debug, Clone)]
pub struct WadoResource {
    pub name: String,
    pub doc_comment: Option<String>,
    pub cm_attr: String,
    pub methods: Vec<WadoFunction>,
}

#[derive(Debug, Clone)]
pub struct WadoInterface {
    pub name: String,
    pub doc_comment: Option<String>,
    pub cm_interface: String,
    pub functions: Vec<WadoFunction>,
}

#[derive(Debug, Clone)]
pub struct WadoFunction {
    pub name: String,
    pub doc_comment: Option<String>,
    pub cm_attr: String,
    pub params: Vec<WadoParam>,
    pub return_type: Option<WadoType>,
    pub is_async: bool,
    pub never_returns: bool,
}

#[derive(Debug, Clone)]
pub struct WadoParam {
    pub name: String,
    pub ty: WadoType,
    /// Original WIT kebab-case name for the `#[cm_params]` attribute
    pub wit_name: String,
}

#[derive(Debug, Clone)]
pub struct WadoWorld {
    pub name: String,
    /// The canonical WIT world name (e.g., "wasi:http/proxy@0.3.0")
    pub canonical_name: String,
    pub doc_comment: Option<String>,
    pub imports: Vec<WadoWorldImport>,
    pub exports: Vec<WadoWorldExport>,
}

#[derive(Debug, Clone)]
pub struct WadoWorldImport {
    pub interface_name: String,
    pub functions: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WadoWorldExport {
    pub name: String,
    pub is_async: bool, // Only world exports use async keyword
    pub params: Vec<WadoParam>,
    pub return_type: Option<WadoType>,
}

/// Wado type representation
#[derive(Debug, Clone)]
pub enum WadoType {
    // Primitives
    Bool,
    Char,
    I8,
    I16,
    I32,
    I64,
    I128,
    U8,
    U16,
    U32,
    U64,
    U128,
    F32,
    F64,
    String,

    // Compound types
    Option(Box<WadoType>),
    Result {
        ok: Option<Box<WadoType>>,
        err: Option<Box<WadoType>>,
    },
    Array(Box<WadoType>),
    Tuple(Vec<WadoType>),
    Stream(Box<WadoType>),
    Future(Box<WadoType>),

    // Named types
    Named(String),

    // References
    Borrow(Box<WadoType>),
}
