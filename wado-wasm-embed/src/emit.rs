//! Emission: the surviving items, renumbered into a dense index space.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::convert::Infallible;

use wasm_encoder::reencode::{Error as ReencodeError, Reencode};
use wasmparser::{ElementItems, ElementKind};

use crate::reach::{Keep, Live};
use crate::{Asset, Embed, Error};

/// What an asset with no memory of its own is given, so every embedded module
/// ends up with the same shape.
const DEFAULT_MEMORY: wasm_encoder::MemoryType = wasm_encoder::MemoryType {
    minimum: 1,
    maximum: None,
    memory64: false,
    shared: false,
    page_size_log2: None,
};

/// Log2 of the default wasm page size, 64 KiB.
const DEFAULT_PAGE_SIZE_LOG2: u32 = 16;

/// Turn a memory the asset described into the request it becomes once the host
/// hands over its own: keep the minimum the asset needs, and drop the ceiling
/// and the spelling of the page size. Both belong to the memory the asset no
/// longer has, and an import declaring them cannot be satisfied by a host
/// memory that leaves them open.
fn shared_memory(mut memory: wasm_encoder::MemoryType) -> wasm_encoder::MemoryType {
    memory.maximum = None;
    if memory.page_size_log2 == Some(DEFAULT_PAGE_SIZE_LOG2) {
        memory.page_size_log2 = None;
    }
    memory
}

pub(crate) fn encode(asset: &Asset<'_>, live: &Live, opts: &Embed<'_>) -> Result<Vec<u8>, Error> {
    assert_eq!(
        asset.funcs.len(),
        asset.bodies.len(),
        "function and code sections must agree"
    );

    let datas = DataPlan::new(asset, live);
    let mut remap = Remap::new(asset, live, &datas);
    let mut module = wasm_encoder::Module::new();

    let mut types = wasm_encoder::TypeSection::new();
    for (i, group) in asset.types.iter().enumerate() {
        if live.types.contains(&(i as u32)) {
            remap.parse_recursive_type_group(types.ty(), group.clone())?;
        }
    }
    if !types.is_empty() {
        module.section(&types);
    }

    // Imports are kept whole; the memory the asset defined becomes one of them,
    // so it shares the component's memory. Index spaces are per-kind, so adding
    // it disturbs nothing else.
    let mut imports = wasm_encoder::ImportSection::new();
    for import in &asset.imports {
        if let wasmparser::TypeRef::Memory(memory) = import.ty {
            let memory = shared_memory(remap.memory_type(memory)?);
            imports.import(import.module, import.name, memory);
        } else {
            remap.parse_import(&mut imports, *import)?;
        }
    }
    if asset.imported.memories == 0 {
        let memory = match asset.memory {
            Some(memory) => shared_memory(remap.memory_type(memory)?),
            None => DEFAULT_MEMORY,
        };
        imports.import(opts.memory_import.0, opts.memory_import.1, memory);
    }
    module.section(&imports);

    let mut funcs = wasm_encoder::FunctionSection::new();
    for (i, ty) in asset.funcs.iter().enumerate() {
        if live.funcs.contains(&(asset.imported.funcs + i as u32)) {
            funcs.function(remap.type_index(*ty)?);
        }
    }
    if !funcs.is_empty() {
        module.section(&funcs);
    }

    let mut tables = wasm_encoder::TableSection::new();
    for (i, table) in asset.tables.iter().enumerate() {
        if live.tables.contains(&(asset.imported.tables + i as u32)) {
            remap.parse_table(&mut tables, table.clone())?;
        }
    }
    if !tables.is_empty() {
        module.section(&tables);
    }

    let mut tags = wasm_encoder::TagSection::new();
    for (i, tag) in asset.tags.iter().enumerate() {
        if live.tags.contains(&(asset.imported.tags + i as u32)) {
            tags.tag(remap.tag_type(*tag)?);
        }
    }
    if !tags.is_empty() {
        module.section(&tags);
    }

    let mut globals = wasm_encoder::GlobalSection::new();
    for (i, global) in asset.globals.iter().enumerate() {
        if live.globals.contains(&(asset.imported.globals + i as u32)) {
            remap.parse_global(&mut globals, global.clone())?;
        }
    }
    if !globals.is_empty() {
        module.section(&globals);
    }

    let mut exports = wasm_encoder::ExportSection::new();
    for export in &asset.exports {
        if (opts.keep_export)(export.name) {
            remap.parse_export(&mut exports, *export)?;
        }
    }
    if !exports.is_empty() {
        module.section(&exports);
    }

    let mut elems = wasm_encoder::ElementSection::new();
    for (i, elem) in asset.elems.iter().enumerate() {
        if !live.elems.contains(&(i as u32)) {
            continue;
        }
        // A declarative segment only makes the functions it names referenceable
        // by `ref.func`, so it is filtered down to the ones that survived.
        if let (ElementKind::Declared, ElementItems::Functions(items)) = (&elem.kind, &elem.items) {
            let mut kept = Vec::new();
            for func in items.clone() {
                let func = func?;
                if live.funcs.contains(&func) {
                    kept.push(remap.function_index(func)?);
                }
            }
            elems.declared(wasm_encoder::Elements::Functions(Cow::Owned(kept)));
        } else {
            remap.parse_element(&mut elems, elem.clone())?;
        }
    }
    if !live.declare.is_empty() {
        let declared = live
            .declare
            .iter()
            .map(|func| remap.function_index(*func))
            .collect::<Result<Vec<_>, _>>()?;
        elems.declared(wasm_encoder::Elements::Functions(Cow::Owned(declared)));
    }
    if !elems.is_empty() {
        module.section(&elems);
    }

    if !asset.datas.is_empty() {
        module.section(&wasm_encoder::DataCountSection { count: datas.total });
    }

    let mut code = wasm_encoder::CodeSection::new();
    for (i, body) in asset.bodies.iter().enumerate() {
        if live.funcs.contains(&(asset.imported.funcs + i as u32)) {
            remap.parse_function_body(&mut code, body.clone())?;
        }
    }
    if !code.is_empty() {
        module.section(&code);
    }

    let mut section = wasm_encoder::DataSection::new();
    for (i, data) in asset.datas.iter().enumerate() {
        match live.datas.get(&(i as u32)) {
            None => {}
            Some(Keep::Whole) => remap.parse_data(&mut section, data.clone())?,
            Some(Keep::Ranges(ranges)) => {
                let base = crate::segment_base(data)
                    .expect("`Keep::Ranges` is only reached for a constant-address segment");
                for range in ranges {
                    let address = base.address + i64::from(range.offset);
                    let offset = if base.is_64 {
                        wasm_encoder::ConstExpr::i64_const(address)
                    } else {
                        wasm_encoder::ConstExpr::i32_const(address as i32)
                    };
                    let bytes = &data.data[range.offset as usize..range.end() as usize];
                    section.active(0, &offset, bytes.iter().copied());
                }
            }
        }
    }
    if !section.is_empty() {
        module.section(&section);
    }

    if !opts.strip_custom_sections {
        if let Some(names) = &asset.names
            && let Some(section) = name_section(names.clone(), &remap)
        {
            module.section(&section);
        }
        for (name, data) in &asset.customs {
            module.section(&wasm_encoder::CustomSection {
                name: Cow::Borrowed(name),
                data: Cow::Borrowed(data),
            });
        }
    }

    Ok(module.finish())
}

/// Carry over the module and function names of what survived.
///
/// No validator reads the `name` section, so an asset can carry a broken one
/// and still be a valid module. That costs the asset its names, not the build.
fn name_section(
    reader: wasmparser::NameSectionReader<'_>,
    remap: &Remap,
) -> Option<wasm_encoder::NameSection> {
    let mut section = wasm_encoder::NameSection::new();
    let mut wrote = false;
    for name in reader {
        match name.ok()? {
            wasmparser::Name::Module { name, .. } => {
                section.module(name);
                wrote = true;
            }
            wasmparser::Name::Function(map) => {
                let mut names = wasm_encoder::NameMap::new();
                let mut kept = false;
                for naming in map {
                    let naming = naming.ok()?;
                    if let Some(index) = remap.funcs.get(naming.index as usize).copied().flatten() {
                        names.append(index, naming.name);
                        kept = true;
                    }
                }
                if kept {
                    section.functions(&names);
                    wrote = true;
                }
            }
            wasmparser::Name::Local(_)
            | wasmparser::Name::Label(_)
            | wasmparser::Name::Type(_)
            | wasmparser::Name::Table(_)
            | wasmparser::Name::Memory(_)
            | wasmparser::Name::Global(_)
            | wasmparser::Name::Element(_)
            | wasmparser::Name::Data(_)
            | wasmparser::Name::Tag(_)
            | wasmparser::Name::Field(_)
            | wasmparser::Name::Unknown { .. } => {}
        }
    }
    wrote.then_some(section)
}

/// Where each source data segment lands once the split ones have become several.
///
/// A split segment is only ever reached through the memory it initialises, so
/// its index is never named; mapping it to its first piece keeps `data.drop` on
/// an already-dropped active segment encodable and costs nothing else.
struct DataPlan {
    first: Vec<Option<u32>>,
    total: u32,
}

impl DataPlan {
    fn new(asset: &Asset<'_>, live: &Live) -> Self {
        let mut first = Vec::with_capacity(asset.datas.len());
        let mut next = 0;
        for index in 0..asset.datas.len() as u32 {
            match live.datas.get(&index) {
                None => first.push(None),
                Some(keep) => {
                    first.push(Some(next));
                    next += match keep {
                        Keep::Whole => 1,
                        Keep::Ranges(ranges) => ranges.len() as u32,
                    };
                }
            }
        }
        DataPlan { first, total: next }
    }
}

/// Old index -> new index, per index space. `None` is an item that was pruned;
/// reaching one means emission and reachability disagreed.
struct Remap {
    funcs: Vec<Option<u32>>,
    tables: Vec<Option<u32>>,
    globals: Vec<Option<u32>>,
    tags: Vec<Option<u32>>,
    types: Vec<Option<u32>>,
    elems: Vec<Option<u32>>,
    datas: Vec<Option<u32>>,
}

impl Remap {
    fn new(asset: &Asset<'_>, live: &Live, datas: &DataPlan) -> Self {
        Remap {
            funcs: entities(asset.total_funcs(), asset.imported.funcs, &live.funcs),
            tables: entities(asset.total_tables(), asset.imported.tables, &live.tables),
            globals: entities(asset.total_globals(), asset.imported.globals, &live.globals),
            tags: entities(asset.total_tags(), asset.imported.tags, &live.tags),
            types: entities(asset.types.len() as u32, 0, &live.types),
            elems: entities(asset.elems.len() as u32, 0, &live.elems),
            datas: datas.first.clone(),
        }
    }
}

/// Imports come first and are always kept; the rest close the gaps left by
/// whatever was pruned.
fn entities(total: u32, imported: u32, live: &BTreeSet<u32>) -> Vec<Option<u32>> {
    let mut map = Vec::with_capacity(total as usize);
    let mut next = 0;
    for index in 0..total {
        if index < imported || live.contains(&index) {
            map.push(Some(next));
            next += 1;
        } else {
            map.push(None);
        }
    }
    map
}

fn lookup(map: &[Option<u32>], index: u32, space: &str) -> u32 {
    map.get(index as usize)
        .copied()
        .flatten()
        .unwrap_or_else(|| panic!("{space} {index} is referenced but was pruned"))
}

impl Reencode for Remap {
    type Error = Infallible;

    fn function_index(&mut self, func: u32) -> Result<u32, ReencodeError> {
        Ok(lookup(&self.funcs, func, "function"))
    }

    fn table_index(&mut self, table: u32) -> Result<u32, ReencodeError> {
        Ok(lookup(&self.tables, table, "table"))
    }

    fn global_index(&mut self, global: u32) -> Result<u32, ReencodeError> {
        Ok(lookup(&self.globals, global, "global"))
    }

    fn tag_index(&mut self, tag: u32) -> Result<u32, ReencodeError> {
        Ok(lookup(&self.tags, tag, "tag"))
    }

    fn type_index(&mut self, ty: u32) -> Result<u32, ReencodeError> {
        Ok(lookup(&self.types, ty, "type"))
    }

    fn element_index(&mut self, element: u32) -> Result<u32, ReencodeError> {
        Ok(lookup(&self.elems, element, "element segment"))
    }

    fn data_index(&mut self, data: u32) -> Result<u32, ReencodeError> {
        Ok(lookup(&self.datas, data, "data segment"))
    }
}
