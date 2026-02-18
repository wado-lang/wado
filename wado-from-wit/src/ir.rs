//! Intermediate representation for Wado code generation

/// A complete Wado module to be generated
#[derive(Debug, Clone)]
pub struct WadoModule {
    pub package_name: String,
    pub package_version: String,
    pub source_files: Vec<String>,
    pub types: Vec<WadoTypeDef>,
    pub resources: Vec<WadoResource>,
    pub effects: Vec<WadoEffect>,
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
            types: Vec::new(),
            resources: Vec::new(),
            effects: Vec::new(),
            standalone_functions: Vec::new(),
            worlds: Vec::new(),
        }
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
    pub wasi_attr: Option<String>,
    pub variants: Vec<WadoEnumVariant>,
}

#[derive(Debug, Clone)]
pub struct WadoEnumVariant {
    pub name: String,
    pub doc_comment: Option<String>,
    /// Original WIT kebab-case name for the `#[wasi("...")]` attribute
    pub wasi_attr: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WadoFlags {
    pub name: String,
    pub doc_comment: Option<String>,
    pub wasi_attr: Option<String>,
    pub flags: Vec<WadoFlagMember>,
}

#[derive(Debug, Clone)]
pub struct WadoFlagMember {
    pub name: String,
    pub doc_comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WadoStruct {
    pub name: String,
    pub doc_comment: Option<String>,
    pub wasi_attr: Option<String>,
    pub fields: Vec<WadoField>,
}

#[derive(Debug, Clone)]
pub struct WadoField {
    pub name: String,
    pub ty: WadoType,
    pub doc_comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WadoVariant {
    pub name: String,
    pub doc_comment: Option<String>,
    pub wasi_attr: Option<String>,
    pub cases: Vec<WadoVariantCase>,
}

#[derive(Debug, Clone)]
pub struct WadoVariantCase {
    pub name: String,
    pub payload: Option<WadoType>,
    pub doc_comment: Option<String>,
    /// Original WIT kebab-case name for the `#[wasi("...")]` attribute
    pub wasi_attr: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WadoNewtype {
    pub name: String,
    pub wasi_attr: Option<String>,
    pub target: WadoType,
}

#[derive(Debug, Clone)]
pub struct WadoResource {
    pub name: String,
    pub doc_comment: Option<String>,
    pub wasi_attr: String,
    pub methods: Vec<WadoFunction>,
}

#[derive(Debug, Clone)]
pub struct WadoEffect {
    pub name: String,
    pub doc_comment: Option<String>,
    pub wasi_interface: String,
    pub functions: Vec<WadoFunction>,
}

#[derive(Debug, Clone)]
pub struct WadoFunction {
    pub name: String,
    pub doc_comment: Option<String>,
    pub wasi_attr: String,
    pub params: Vec<WadoParam>,
    pub return_type: Option<WadoType>,
    pub is_async: bool,
    pub never_returns: bool,
}

#[derive(Debug, Clone)]
pub struct WadoParam {
    pub name: String,
    pub ty: WadoType,
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
    pub effect_name: String,
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
