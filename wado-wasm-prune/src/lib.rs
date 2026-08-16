//! Rewrite an embedded core wasm asset so a component can host it.
//!
//! Two transformations, one pass:
//!
//! - the asset's memory definition becomes an import, so the asset shares the
//!   component's memory instead of defining its own;
//! - every function, global, table, tag, type and segment unreachable from the
//!   exports the component actually uses is dropped.
//!
//! Custom sections do not survive: the asset's own DWARF describes the
//! pre-prune index space, and only the `name` section is rebuilt (filtered and
//! remapped) because stack traces read it.

mod emit;
mod reach;

use std::fmt;

/// How to rewrite an asset.
pub struct Rewrite<'a> {
    /// `(module, name)` the memory is imported under.
    pub memory_import: (&'a str, &'a str),
    /// The exports to keep. Everything unreachable from them is dropped.
    pub keep_export: &'a dyn Fn(&str) -> bool,
}

#[derive(Debug)]
pub enum Error {
    /// The asset is not a core wasm module this rewrite can read.
    Parse(wasmparser::BinaryReaderError),
    /// The asset has a shape a component cannot host.
    Unsupported(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Parse(e) => write!(f, "failed to parse wasm asset: {e}"),
            Error::Unsupported(what) => write!(f, "unsupported wasm asset: {what}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<wasmparser::BinaryReaderError> for Error {
    fn from(e: wasmparser::BinaryReaderError) -> Self {
        Error::Parse(e)
    }
}

impl From<wasm_encoder::reencode::Error> for Error {
    fn from(e: wasm_encoder::reencode::Error) -> Self {
        use wasm_encoder::reencode::Error as E;
        match e {
            E::ParseError(e) => Error::Parse(e),
            E::InvalidConstExpr => Error::Unsupported("invalid constant expression"),
            E::InvalidCodeSectionSize => Error::Unsupported("invalid code section size"),
            E::CanonicalizedHeapTypeReference => {
                Error::Unsupported("canonicalized heap type reference")
            }
            E::UnexpectedNonCoreModuleSection
            | E::UnexpectedNonComponentSection
            | E::UnsupportedCoreTypeInComponent => Error::Unsupported("not a core wasm module"),
            E::UserError(e) => match e {},
        }
    }
}

/// Rewrite `wasm`, keeping only what `opts` asks for.
///
/// `wasm` must be a valid core module: indices are taken at face value, so an
/// out-of-range one panics rather than producing a module that only looks
/// right. Validate before calling.
pub fn rewrite(wasm: &[u8], opts: &Rewrite<'_>) -> Result<Vec<u8>, Error> {
    let asset = Asset::collect(wasm)?;
    let live = reach::live(&asset, opts.keep_export)?;
    emit::encode(&asset, &live, opts.keep_export, opts.memory_import)
}

/// Everything the rewrite needs from the asset, in its original index space.
#[derive(Default)]
pub(crate) struct Asset<'a> {
    pub types: Vec<wasmparser::RecGroup>,
    pub imports: Vec<wasmparser::Import<'a>>,
    pub imported: Imported,
    /// Type index per defined function.
    pub funcs: Vec<u32>,
    pub tables: Vec<wasmparser::Table<'a>>,
    /// The defined memory, if the asset has one of its own.
    pub memory: Option<wasmparser::MemoryType>,
    pub tags: Vec<wasmparser::TagType>,
    pub globals: Vec<wasmparser::Global<'a>>,
    pub exports: Vec<wasmparser::Export<'a>>,
    pub elems: Vec<wasmparser::Element<'a>>,
    pub datas: Vec<wasmparser::Data<'a>>,
    pub bodies: Vec<wasmparser::FunctionBody<'a>>,
    pub names: Option<wasmparser::NameSectionReader<'a>>,
}

/// How many of each index space the imports occupy. Imports are kept whole, so
/// these are also the first indices of the pruned module.
#[derive(Default)]
pub(crate) struct Imported {
    pub funcs: u32,
    pub tables: u32,
    pub memories: u32,
    pub globals: u32,
    pub tags: u32,
}

impl<'a> Asset<'a> {
    fn collect(wasm: &'a [u8]) -> Result<Self, Error> {
        use wasmparser::{Payload, TypeRef};

        let mut asset = Asset::default();
        for payload in wasmparser::Parser::new(0).parse_all(wasm) {
            match payload? {
                Payload::TypeSection(reader) => {
                    for group in reader {
                        let group = group?;
                        // One sub type per group keeps type indices equal to
                        // group indices. Assets carry plain function types;
                        // anything recursive is a GC module the loader rejects.
                        if group.types().len() != 1 {
                            return Err(Error::Unsupported("recursive type group"));
                        }
                        asset.types.push(group);
                    }
                }
                Payload::ImportSection(reader) => {
                    for import in reader.into_imports() {
                        let import = import?;
                        match import.ty {
                            TypeRef::Func(_) | TypeRef::FuncExact(_) => asset.imported.funcs += 1,
                            TypeRef::Table(_) => asset.imported.tables += 1,
                            TypeRef::Memory(_) => asset.imported.memories += 1,
                            TypeRef::Global(_) => asset.imported.globals += 1,
                            TypeRef::Tag(_) => asset.imported.tags += 1,
                        }
                        asset.imports.push(import);
                    }
                }
                Payload::FunctionSection(reader) => {
                    for ty in reader {
                        asset.funcs.push(ty?);
                    }
                }
                Payload::TableSection(reader) => {
                    for table in reader {
                        asset.tables.push(table?);
                    }
                }
                Payload::MemorySection(reader) => {
                    for memory in reader {
                        if asset.memory.is_some() {
                            return Err(Error::Unsupported("more than one memory"));
                        }
                        asset.memory = Some(memory?);
                    }
                }
                Payload::TagSection(reader) => {
                    for tag in reader {
                        asset.tags.push(tag?);
                    }
                }
                Payload::GlobalSection(reader) => {
                    for global in reader {
                        asset.globals.push(global?);
                    }
                }
                Payload::ExportSection(reader) => {
                    for export in reader {
                        asset.exports.push(export?);
                    }
                }
                Payload::ElementSection(reader) => {
                    for elem in reader {
                        asset.elems.push(elem?);
                    }
                }
                Payload::DataSection(reader) => {
                    for data in reader {
                        asset.datas.push(data?);
                    }
                }
                Payload::CodeSectionEntry(body) => asset.bodies.push(body),
                Payload::CustomSection(reader) if reader.name() == "name" => {
                    asset.names = Some(wasmparser::NameSectionReader::new(
                        wasmparser::BinaryReader::new(reader.data(), reader.data_offset()),
                    ));
                }
                // A start section runs at instantiation time, which the
                // embedding cannot honour; the loader rejects it before here.
                Payload::StartSection { .. } => {
                    return Err(Error::Unsupported("start section"));
                }
                Payload::UnknownSection { .. } => {
                    return Err(Error::Unsupported("unknown section"));
                }
                // Section headers, the module envelope, other custom sections,
                // and component payloads: nothing to carry over.
                _ => {}
            }
        }

        if asset.imported.memories + u32::from(asset.memory.is_some()) > 1 {
            return Err(Error::Unsupported("more than one memory"));
        }
        Ok(asset)
    }

    pub fn total_funcs(&self) -> u32 {
        self.imported.funcs + self.funcs.len() as u32
    }

    pub fn total_tables(&self) -> u32 {
        self.imported.tables + self.tables.len() as u32
    }

    pub fn total_globals(&self) -> u32 {
        self.imported.globals + self.globals.len() as u32
    }

    pub fn total_tags(&self) -> u32 {
        self.imported.tags + self.tags.len() as u32
    }
}
