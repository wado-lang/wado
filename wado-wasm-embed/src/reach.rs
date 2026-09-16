//! Reachability: what the kept exports can still reach.
//!
//! No instruction is decoded by hand here. `wasm-encoder`'s reencoder routes
//! every index of every operator, constant expression and segment through the
//! [`Reencode`] hooks, so recording those hooks yields the whole reference
//! graph — and keeps yielding it as new opcodes land.

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;

use wasm_encoder::reencode::{Error as ReencodeError, Reencode};
use wasmparser::{ElementItems, ElementKind, ExternalKind};

use crate::dataref::{DataRange, DataRefs, Target, merge_with_gap};
use crate::{Asset, Embed, Error, segment_base};

/// A gap this small is cheaper to keep than to split around: a second segment
/// costs a header, an offset expression and a length.
const SEGMENT_HEADER_BYTES: u32 = 12;

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
    pub datas: BTreeMap<u32, Keep>,
    /// Functions a surviving `ref.func` names that nothing left in the module
    /// declares. They need a declarative segment of their own.
    pub declare: Vec<u32>,
    /// Functions kept for their name and signature alone.
    pub stub: BTreeSet<u32>,
}

/// How much of a surviving data segment survives.
pub(crate) enum Keep {
    Whole,
    /// Sorted, merged, non-empty byte ranges, each emitted as a segment of its
    /// own at the matching address.
    Ranges(Vec<DataRange>),
}

pub(crate) fn live(asset: &Asset<'_>, opts: &Embed<'_>) -> Result<Live, Error> {
    let keep_export = opts.keep_export;
    let (data_refs, pointers) = match &asset.data_refs {
        Some(refs) => resolve_map(asset, refs)?,
        None => (BTreeMap::new(), Vec::new()),
    };
    let mut walk = Walk {
        asset,
        live: Live::default(),
        queue: Vec::new(),
        ref_funcs: BTreeSet::new(),
        data_refs,
        pointers,
        // Only an active segment at a constant base can have its pieces name
        // their own addresses. A passive one is reached through the
        // `memory.init` that copies it, which the walk already follows.
        splittable: asset
            .datas
            .iter()
            .map(|data| asset.data_refs.is_some() && segment_base(data).is_some())
            .collect(),
    };

    // An imported table is kept whole, so the segments filling it are live for
    // the same reason active data segments are.
    for index in 0..asset.imported.tables {
        walk.mark_table(index);
    }

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
        // A stubbed export keeps its index and its type but roots nothing, so
        // its body is never walked and whatever only it reached is collected.
        if (opts.stub_export)(export.name)
            && matches!(export.kind, ExternalKind::Func | ExternalKind::FuncExact)
        {
            walk.mark_stub(export.index);
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
        // An active segment initialises the shared memory whether or not
        // anything still reads what it wrote, so it is live by default unless a
        // map can narrow it.
        if matches!(data.kind, wasmparser::DataKind::Active { .. }) && !walk.splittable[i] {
            walk.mark_data_whole(i as u32);
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

    let ref_funcs = walk.ref_funcs;
    let mut live = walk.live;
    for keep in live.datas.values_mut() {
        if let Keep::Ranges(ranges) = keep {
            merge_with_gap(ranges, SEGMENT_HEADER_BYTES);
        }
    }
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

    // `ref.func` only validates for a function the module declares, and an
    // export, a global, a table or any element segment is a declaration. The
    // prune can take the last one away while the function itself lives on, so
    // whatever is left undeclared gets a declarative segment at emission.
    let mut declared: BTreeSet<u32> = BTreeSet::new();
    for export in &asset.exports {
        if matches!(export.kind, ExternalKind::Func | ExternalKind::FuncExact)
            && keep_export(export.name)
        {
            declared.insert(export.index);
        }
    }
    for index in &live.elems {
        // Expression items are opaque here; missing one only costs a redundant
        // declaration, never a missing one.
        if let ElementItems::Functions(items) = &asset.elems[*index as usize].items {
            for func in items.clone() {
                declared.insert(func?);
            }
        }
    }
    live.declare = ref_funcs.difference(&declared).copied().collect();
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
    /// Targets of every `ref.func` reached, live by construction.
    ref_funcs: BTreeSet<u32>,
    /// The data ranges each function reads, empty where the asset carries no
    /// map. Reaching a function reaches its ranges, so the data edges close
    /// over the same worklist the code edges do.
    data_refs: BTreeMap<u32, &'a [DataRange]>,
    /// Where each pointer stored in the data sits, and what it reaches. Keeping
    /// the bytes a pointer occupies reaches its target, which is what lets a
    /// live data range root a function.
    pointers: Vec<Edge>,
    /// Which segments a range may narrow, by segment index.
    splittable: Vec<bool>,
}

/// A pointer site with its target resolved against the asset's own indices.
struct Edge {
    segment: u32,
    offset: u32,
    target: Reached,
    fired: bool,
}

enum Reached {
    Data(DataRange),
    Func(u32),
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
        self.ref_funcs.extend(refs.ref_funcs);
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
            self.mark_data_whole(i);
        }
        for i in refs.types {
            self.mark_type(i);
        }
        Ok(())
    }

    fn mark_func(&mut self, index: u32) {
        // A stub already sits in `live.funcs`, so that set alone cannot say
        // whether the body has been walked.
        let was_stub = self.live.stub.remove(&index);
        if !self.live.funcs.insert(index) && !was_stub {
            return;
        }
        self.queue.push(Item::Func(index));
        for range in self.data_refs.get(&index).copied().unwrap_or_default() {
            self.mark_data_range(*range);
        }
    }

    /// Keep a function's slot and signature without walking its body. Its type
    /// stays live: the export and the body left behind both still name it.
    fn mark_stub(&mut self, index: u32) {
        if self.live.funcs.contains(&index) {
            return;
        }
        if let Some(defined) = self.defined(index, self.asset.imported.funcs) {
            self.mark_type(self.asset.funcs[defined]);
        }
        self.live.funcs.insert(index);
        self.live.stub.insert(index);
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

    /// Keep every byte of a segment: nothing says which of them are still read.
    fn mark_data_whole(&mut self, index: u32) {
        if self.live.datas.insert(index, Keep::Whole).is_none() {
            self.queue.push(Item::Data(index));
        }
        self.fire(|edge| edge.segment == index);
    }

    /// Follow every not-yet-followed pointer whose site `covers` keeps. An edge
    /// fires once, so a chain of pointers costs one step each however many
    /// ranges cover it.
    fn fire<F: Fn(&Edge) -> bool>(&mut self, covers: F) {
        let mut reached = Vec::new();
        for edge in &mut self.pointers {
            if !edge.fired && covers(edge) {
                edge.fired = true;
                reached.push(match edge.target {
                    Reached::Data(range) => Reached::Data(range),
                    Reached::Func(index) => Reached::Func(index),
                });
            }
        }
        for target in reached {
            match target {
                Reached::Data(range) => self.mark_data_range(range),
                Reached::Func(index) => self.mark_func(index),
            }
        }
    }

    /// Keep one range of a segment, and queue the segment so its offset
    /// expression is walked. A range naming a segment no map can narrow is
    /// dropped: something else already decided that segment's fate.
    fn mark_data_range(&mut self, range: DataRange) {
        assert!(
            (range.segment as usize) < self.splittable.len(),
            "`func_ranges` rejects a map naming a segment the asset does not have"
        );
        if !self.splittable[range.segment as usize] {
            return;
        }
        match self.live.datas.entry(range.segment) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(Keep::Ranges(vec![range]));
                self.queue.push(Item::Data(range.segment));
            }
            std::collections::btree_map::Entry::Occupied(mut slot) => match slot.get_mut() {
                Keep::Whole => {}
                Keep::Ranges(ranges) => ranges.push(range),
            },
        }
        self.fire(|edge| {
            edge.segment == range.segment
                && range.offset <= edge.offset
                && edge.offset < range.end()
        });
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
    ref_funcs: Vec<u32>,
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

    /// `call x` and `ref.func x` both reach `function_index`, but only the
    /// second one needs `x` declared, so it is picked out here.
    fn parse_instruction<'a>(
        &mut self,
        reader: &mut wasmparser::OperatorsReader<'a>,
    ) -> Result<wasm_encoder::Instruction<'a>, ReencodeError> {
        let instruction = wasm_encoder::reencode::utils::parse_instruction(self, reader)?;
        if let wasm_encoder::Instruction::RefFunc(index) = instruction {
            self.0.ref_funcs.push(index);
        }
        Ok(instruction)
    }

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

/// Resolve the asset's map against its `name` section, which both halves of the
/// map are keyed by, so it is walked once.
///
/// Anything the module cannot answer for — a name landing on no function, a
/// range outside its segment — means the map has drifted from it. Honouring a
/// drifted map prunes data something still reads, so it is an error.
fn resolve_map<'a>(
    asset: &'a Asset<'_>,
    refs: &'a DataRefs,
) -> Result<(BTreeMap<u32, &'a [DataRange]>, Vec<Edge>), Error> {
    let names = asset.names.clone().ok_or_else(|| {
        Error::DataRef("the asset has no `name` section to resolve the map against".into())
    })?;
    let mut funcs: BTreeMap<&str, u32> = BTreeMap::new();
    for subsection in names {
        if let wasmparser::Name::Function(map) = subsection? {
            for naming in map {
                let naming = naming?;
                funcs.insert(naming.name, naming.index);
            }
        }
    }

    let mut reads = BTreeMap::new();
    for (name, index) in &funcs {
        if let Some(ranges) = refs.get(name) {
            for range in ranges {
                check_range(asset, range, &format!("function `{name}`"))?;
            }
            reads.insert(*index, ranges);
        }
    }
    if reads.len() != refs.len() {
        return Err(Error::DataRef(format!(
            "names {} functions, of which the asset's `name` section resolves {}",
            refs.len(),
            reads.len()
        )));
    }

    let mut edges = Vec::new();
    for pointer in refs.pointers() {
        let site = DataRange::at(pointer.segment, pointer.offset).ok_or_else(|| {
            Error::DataRef(format!(
                "a pointer sits at {}:{}, which no segment can reach",
                pointer.segment, pointer.offset
            ))
        })?;
        check_range(asset, &site, "a pointer")?;
        let target = match &pointer.target {
            Target::Data(range) => {
                check_range(asset, range, "a pointer")?;
                Reached::Data(*range)
            }
            Target::Func(name) => Reached::Func(*funcs.get(name.as_str()).ok_or_else(|| {
                Error::DataRef(format!(
                    "a pointer reaches `{name}`, which the asset's `name` section does not name"
                ))
            })?),
        };
        edges.push(Edge {
            segment: pointer.segment,
            offset: pointer.offset,
            target,
            fired: false,
        });
    }
    Ok((reads, edges))
}

fn check_range(asset: &Asset<'_>, range: &DataRange, reader: &str) -> Result<(), Error> {
    let segment = asset.datas.get(range.segment as usize).ok_or_else(|| {
        Error::DataRef(format!(
            "{reader} reads segment {}, which the asset does not have",
            range.segment
        ))
    })?;
    if range.end() as usize > segment.data.len() {
        return Err(Error::DataRef(format!(
            "{reader} reads {}..{} of segment {}, which is {} bytes",
            range.offset,
            range.end(),
            range.segment,
            segment.data.len()
        )));
    }
    Ok(())
}
