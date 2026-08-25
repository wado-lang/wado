//! Prepare a core wasm asset for embedding in a component.
//!
//! Two transformations, one pass:
//!
//! - the asset's memory definition becomes an import, so the asset shares the
//!   component's memory instead of defining its own;
//! - every function, global, table, tag, type and segment unreachable from the
//!   exports the component actually uses is dropped.
//!
//! An active data segment is pruned by the byte where the asset carries a
//! [`dataref`] map, and kept whole where it does not: a segment initialises
//! memory whether or not anything still reads what it wrote.
//!
//! A custom section survives only where the prune leaves it true. The `name`
//! section is rebuilt against the functions that remain; `producers` and
//! `target_features` name no index and pass through untouched. Everything else
//! goes: DWARF, `linking` and `reloc.*` describe an index space that no longer
//! exists, `wado.dataref` describes segments the prune has since split, branch
//! hints carry byte offsets that renumbering moves (a narrower index encodes
//! shorter), and an unknown section cannot be shown to have survived at all.
//! `strip_custom_sections` drops even the ones that did.

pub mod dataref;
mod emit;
mod reach;

use std::fmt;

/// How to embed an asset.
pub struct Embed<'a> {
    /// `(module, name)` the memory is imported under.
    pub memory_import: (&'a str, &'a str),
    /// The exports to keep. Everything unreachable from them is dropped.
    pub keep_export: &'a dyn Fn(&str) -> bool,
    /// Drop every custom section, the `name` section included (`-Os`).
    pub strip_custom_sections: bool,
}

/// Custom sections that name no index, so the prune cannot make them lie.
pub(crate) const INDEX_FREE_CUSTOM_SECTIONS: [&str; 2] = ["producers", "target_features"];

#[derive(Debug)]
pub enum Error {
    /// The asset is not a core wasm module this pass can read.
    Parse(wasmparser::BinaryReaderError),
    /// The asset has a shape a component cannot host.
    Unsupported(&'static str),
    /// The asset's data-reference map cannot be read, or does not describe it.
    DataRef(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Parse(e) => write!(f, "failed to parse wasm asset: {e}"),
            Error::Unsupported(what) => write!(f, "unsupported wasm asset: {what}"),
            Error::DataRef(why) => write!(f, "invalid `{}` section: {why}", dataref::SECTION_NAME),
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

/// The constant address an active data segment starts at.
pub(crate) struct SegmentBase {
    pub address: i64,
    pub is_64: bool,
}

/// `None` for a passive segment or one at a computed address — neither can hand
/// the pieces of a split an address of their own.
pub(crate) fn segment_base(data: &wasmparser::Data<'_>) -> Option<SegmentBase> {
    let wasmparser::DataKind::Active { offset_expr, .. } = &data.kind else {
        return None;
    };
    let mut reader = offset_expr.get_operators_reader();
    let base = match reader.read() {
        Ok(wasmparser::Operator::I32Const { value }) => SegmentBase {
            address: i64::from(value),
            is_64: false,
        },
        Ok(wasmparser::Operator::I64Const { value }) => SegmentBase {
            address: value,
            is_64: true,
        },
        _ => return None,
    };
    reader.is_end_then_eof().then_some(base)
}

/// Embed `wasm`, keeping only what `opts` asks for.
///
/// `wasm` must be a valid core module: indices are taken at face value, so an
/// out-of-range one panics rather than producing a module that only looks
/// right. Validate before calling.
pub fn embed(wasm: &[u8], opts: &Embed<'_>) -> Result<Vec<u8>, Error> {
    let asset = Asset::collect(wasm)?;
    let live = reach::live(&asset, opts.keep_export)?;
    emit::encode(&asset, &live, opts)
}

/// Everything the pass needs from the asset, in its original index space.
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
    /// Which bytes of the data segments each function reads, when the asset
    /// says. `None` keeps every active segment whole.
    pub data_refs: Option<dataref::DataRefs>,
    /// The custom sections that outlive the prune, in section order.
    pub customs: Vec<(&'a str, &'a [u8])>,
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
                Payload::CustomSection(reader) if reader.name() == dataref::SECTION_NAME => {
                    let text = str::from_utf8(reader.data())
                        .map_err(|e| Error::DataRef(format!("not UTF-8: {e}")))?;
                    asset.data_refs = Some(dataref::DataRefs::parse(text)?);
                }
                Payload::CustomSection(reader)
                    if INDEX_FREE_CUSTOM_SECTIONS.contains(&reader.name()) =>
                {
                    asset.customs.push((reader.name(), reader.data()));
                }
                // A start section runs at instantiation time, which the
                // embedding cannot honour; the loader rejects it before here.
                Payload::StartSection { .. } => {
                    return Err(Error::Unsupported("start section"));
                }
                Payload::UnknownSection { .. } => {
                    return Err(Error::Unsupported("unknown section"));
                }
                // Section headers, the module envelope, the custom sections the
                // prune invalidates, and component payloads: nothing to carry
                // over.
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
