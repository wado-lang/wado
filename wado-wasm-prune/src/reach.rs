//! Reachability: what the kept exports can still reach.
//!
//! No instruction is decoded by hand here. `wasm-encoder`'s reencoder routes
//! every index of every operator, constant expression and segment through the
//! [`Reencode`] hooks, so recording those hooks yields the whole reference
//! graph — and keeps yielding it as new opcodes land.

use std::collections::BTreeSet;
use std::convert::Infallible;

use wasm_encoder::reencode::{Error as ReencodeError, Reencode};
use wasmparser::{ElementItems, ElementKind, ExternalKind};

use crate::{Asset, Error};

/// What survives, in the asset's original index space. Imports are not listed:
/// they are kept whole.
#[derive(Default)]
pub(crate) struct Live {
    pub funcs: BTreeSet<u32>,
    pub tables: BTreeSet<u32>,
    pub globals: BTreeSet<u32>,
    pub tags: BTreeSet<u32>,
    pub types: BTreeSet<u32>,
    pub elems: BTreeSet<u32>,
    pub datas: BTreeSet<u32>,
}

pub(crate) fn live(asset: &Asset<'_>, keep_export: &dyn Fn(&str) -> bool) -> Result<Live, Error> {
    let mut walk = Walk {
        asset,
        live: Live::default(),
        queue: Vec::new(),
    };

    // Imports are kept whole, so the types they name are live from the start.
    for import in &asset.imports {
        let import = *import;
        walk.record(|r| {
            let mut sink = wasm_encoder::ImportSection::new();
            r.parse_import(&mut sink, import)
        })?;
    }

    for export in &asset.exports {
        if !keep_export(export.name) {
            continue;
        }
        match export.kind {
            ExternalKind::Func | ExternalKind::FuncExact => walk.mark_func(export.index),
            ExternalKind::Table => walk.mark_table(export.index),
            ExternalKind::Global => walk.mark_global(export.index),
            ExternalKind::Tag => walk.mark_tag(export.index),
            // The component supplies the memory; an export of it is dropped.
            ExternalKind::Memory => {}
        }
    }

    for (i, data) in asset.datas.iter().enumerate() {
        // Active segments initialise the shared memory whether or not anything
        // still reads what they wrote.
        if matches!(data.kind, wasmparser::DataKind::Active { .. }) {
            walk.mark_data(i as u32);
        }
    }

    for (i, elem) in asset.elems.iter().enumerate() {
        // A declarative segment holding expressions cannot be filtered item by
        // item, so it is kept whole and everything it names stays with it.
        if matches!(elem.kind, ElementKind::Declared)
            && matches!(elem.items, ElementItems::Expressions(..))
        {
            walk.mark_elem(i as u32);
        }
    }

    walk.run()?;

    let mut live = walk.live;
    // A declarative segment of function indices is filtered down to the
    // functions that survived, and dropped when none did.
    for (i, elem) in asset.elems.iter().enumerate() {
        if let (ElementKind::Declared, ElementItems::Functions(items)) = (&elem.kind, &elem.items) {
            let mut any = false;
            for func in items.clone() {
                any |= live.funcs.contains(&func?);
            }
            if any {
                live.elems.insert(i as u32);
            }
        }
    }
    Ok(live)
}

enum Item {
    Func(u32),
    Table(u32),
    Global(u32),
    Tag(u32),
    Elem(u32),
    Data(u32),
    Type(u32),
}

struct Walk<'a, 'b> {
    asset: &'a Asset<'b>,
    live: Live,
    queue: Vec<Item>,
}

impl Walk<'_, '_> {
    fn run(&mut self) -> Result<(), Error> {
        while let Some(item) = self.queue.pop() {
            match item {
                Item::Func(i) => self.visit_func(i)?,
                Item::Table(i) => self.visit_table(i)?,
                Item::Global(i) => self.visit_global(i)?,
                Item::Elem(i) => self.visit_elem(i)?,
                Item::Data(i) => self.visit_data(i)?,
                Item::Type(i) => self.visit_type(i)?,
                Item::Tag(i) => self.visit_tag(i),
            }
        }
        Ok(())
    }

    fn visit_func(&mut self, index: u32) -> Result<(), Error> {
        let Some(defined) = self.defined(index, self.asset.imported.funcs) else {
            return Ok(());
        };
        self.mark_type(self.asset.funcs[defined]);
        let body = self.asset.bodies[defined].clone();
        self.record(|r| {
            let mut sink = wasm_encoder::CodeSection::new();
            r.parse_function_body(&mut sink, body)
        })
    }

    fn visit_table(&mut self, index: u32) -> Result<(), Error> {
        // An active segment writes into the table, so anything it names is
        // reachable through `call_indirect` once the table itself is.
        for (i, elem) in self.asset.elems.iter().enumerate() {
            if let ElementKind::Active { table_index, .. } = elem.kind
                && table_index.unwrap_or(0) == index
            {
                self.mark_elem(i as u32);
            }
        }
        let Some(defined) = self.defined(index, self.asset.imported.tables) else {
            return Ok(());
        };
        let table = self.asset.tables[defined].clone();
        self.record(|r| {
            let mut sink = wasm_encoder::TableSection::new();
            r.parse_table(&mut sink, table)
        })
    }

    fn visit_global(&mut self, index: u32) -> Result<(), Error> {
        let Some(defined) = self.defined(index, self.asset.imported.globals) else {
            return Ok(());
        };
        let global = self.asset.globals[defined].clone();
        self.record(|r| {
            let mut sink = wasm_encoder::GlobalSection::new();
            r.parse_global(&mut sink, global)
        })
    }

    fn visit_tag(&mut self, index: u32) {
        if let Some(defined) = self.defined(index, self.asset.imported.tags) {
            self.mark_type(self.asset.tags[defined].func_type_idx);
        }
    }

    /// A type can name other types — a parameter of `(ref $t)`, a supertype —
    /// so the types a live type needs are live too.
    fn visit_type(&mut self, index: u32) -> Result<(), Error> {
        let sub_type = self.asset.types[index as usize]
            .clone()
            .into_types()
            .next()
            .expect("collect keeps one type per group");
        self.record(|r| r.sub_type(sub_type).map(|_| ()))
    }

    fn visit_elem(&mut self, index: u32) -> Result<(), Error> {
        let elem = self.asset.elems[index as usize].clone();
        self.record(|r| {
            let mut sink = wasm_encoder::ElementSection::new();
            r.parse_element(&mut sink, elem)
        })
    }

    fn visit_data(&mut self, index: u32) -> Result<(), Error> {
        let data = self.asset.datas[index as usize].clone();
        self.record(|r| {
            let mut sink = wasm_encoder::DataSection::new();
            r.parse_data(&mut sink, data)
        })
    }

    /// `None` for an imported index, which has nothing of its own to walk.
    fn defined(&self, index: u32, imported: u32) -> Option<usize> {
        index.checked_sub(imported).map(|i| i as usize)
    }

    fn record<F>(&mut self, f: F) -> Result<(), Error>
    where
        F: FnOnce(&mut Recorder<'_>) -> Result<(), ReencodeError>,
    {
        let mut refs = Refs::default();
        f(&mut Recorder(&mut refs))?;
        for i in refs.funcs {
            self.mark_func(i);
        }
        for i in refs.tables {
            self.mark_table(i);
        }
        for i in refs.globals {
            self.mark_global(i);
        }
        for i in refs.tags {
            self.mark_tag(i);
        }
        for i in refs.elems {
            self.mark_elem(i);
        }
        for i in refs.datas {
            self.mark_data(i);
        }
        for i in refs.types {
            self.mark_type(i);
        }
        Ok(())
    }

    fn mark_func(&mut self, index: u32) {
        if self.live.funcs.insert(index) {
            self.queue.push(Item::Func(index));
        }
    }

    fn mark_table(&mut self, index: u32) {
        if self.live.tables.insert(index) {
            self.queue.push(Item::Table(index));
        }
    }

    fn mark_global(&mut self, index: u32) {
        if self.live.globals.insert(index) {
            self.queue.push(Item::Global(index));
        }
    }

    fn mark_tag(&mut self, index: u32) {
        if self.live.tags.insert(index) {
            self.queue.push(Item::Tag(index));
        }
    }

    fn mark_elem(&mut self, index: u32) {
        if self.live.elems.insert(index) {
            self.queue.push(Item::Elem(index));
        }
    }

    fn mark_data(&mut self, index: u32) {
        if self.live.datas.insert(index) {
            self.queue.push(Item::Data(index));
        }
    }

    fn mark_type(&mut self, index: u32) {
        if self.live.types.insert(index) {
            self.queue.push(Item::Type(index));
        }
    }
}

/// Every index one item refers to, in the order the reencoder walks them.
#[derive(Default)]
struct Refs {
    funcs: Vec<u32>,
    tables: Vec<u32>,
    globals: Vec<u32>,
    tags: Vec<u32>,
    types: Vec<u32>,
    elems: Vec<u32>,
    datas: Vec<u32>,
}

/// A reencoder that keeps every index as it is and writes down what it saw.
/// Its output is thrown away; the record is the point.
struct Recorder<'a>(&'a mut Refs);

impl Reencode for Recorder<'_> {
    type Error = Infallible;

    fn function_index(&mut self, func: u32) -> Result<u32, ReencodeError> {
        self.0.funcs.push(func);
        Ok(func)
    }

    fn table_index(&mut self, table: u32) -> Result<u32, ReencodeError> {
        self.0.tables.push(table);
        Ok(table)
    }

    fn global_index(&mut self, global: u32) -> Result<u32, ReencodeError> {
        self.0.globals.push(global);
        Ok(global)
    }

    fn tag_index(&mut self, tag: u32) -> Result<u32, ReencodeError> {
        self.0.tags.push(tag);
        Ok(tag)
    }

    fn type_index(&mut self, ty: u32) -> Result<u32, ReencodeError> {
        self.0.types.push(ty);
        Ok(ty)
    }

    fn element_index(&mut self, element: u32) -> Result<u32, ReencodeError> {
        self.0.elems.push(element);
        Ok(element)
    }

    fn data_index(&mut self, data: u32) -> Result<u32, ReencodeError> {
        self.0.datas.push(data);
        Ok(data)
    }
}
