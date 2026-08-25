//! Which bytes of an asset's data segments each of its functions reads, keyed
//! by function name and carried in a [`SECTION_NAME`] custom section.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::Error;

/// The custom section an asset carries its data-reference map in.
pub const SECTION_NAME: &str = "wado.dataref";

/// A half-open byte range of one data segment.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct DataRange {
    pub segment: u32,
    pub offset: u32,
    pub size: u32,
}

impl DataRange {
    /// `None` for a range with no end to reach. Every constructor goes through
    /// here, so [`end`](Self::end) is total.
    fn new(segment: u32, offset: u32, size: u32) -> Option<Self> {
        offset.checked_add(size)?;
        Some(DataRange {
            segment,
            offset,
            size,
        })
    }

    pub(crate) fn end(&self) -> u32 {
        self.offset
            .checked_add(self.size)
            .expect("a DataRange without an end is rejected at construction")
    }
}

/// Function name -> the data ranges its body relocates against. Names are the
/// asset's own, so a map and the module it describes only mean anything together.
#[derive(Debug)]
pub struct DataRefs {
    entries: BTreeMap<String, Vec<DataRange>>,
}

impl DataRefs {
    pub fn get(&self, func_name: &str) -> Option<&[DataRange]> {
        self.entries.get(func_name).map(Vec::as_slice)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Bytes the map claims, counting overlapping ranges once. A reviewer reads
    /// this against the segment's size: what is unclaimed is padding.
    pub fn claimed_bytes(&self) -> u32 {
        let mut ranges: Vec<DataRange> = self.entries.values().flatten().copied().collect();
        merge(&mut ranges);
        ranges.iter().map(|r| r.size).sum()
    }

    /// Parse the payload of a [`SECTION_NAME`] section: one function per line,
    /// `<name> <segment>:<offset>+<size> ...`, fields separated by any run of
    /// spaces so the emitted form can align its columns.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut entries = BTreeMap::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let malformed = || Error::DataRef(format!("line {}: malformed: {line}", number + 1));
            let mut fields = line.split_ascii_whitespace();
            let name = fields.next().ok_or_else(malformed)?;
            let mut ranges = Vec::new();
            for field in fields {
                let (segment, rest) = field.split_once(':').ok_or_else(malformed)?;
                let (offset, size) = rest.split_once('+').ok_or_else(malformed)?;
                ranges.push(
                    DataRange::new(
                        segment.parse().map_err(|_| malformed())?,
                        offset.parse().map_err(|_| malformed())?,
                        size.parse().map_err(|_| malformed())?,
                    )
                    .ok_or_else(malformed)?,
                );
            }
            if ranges.is_empty() {
                return Err(malformed());
            }
            merge(&mut ranges);
            if entries.insert(name.to_string(), ranges).is_some() {
                return Err(Error::DataRef(format!("`{name}` is listed twice")));
            }
        }
        // Indistinguishable from a map that failed to resolve, and honouring it
        // prunes every data segment away. An asset with nothing to say carries
        // no section at all.
        if entries.is_empty() {
            return Err(Error::DataRef("names no function".into()));
        }
        Ok(DataRefs { entries })
    }

    /// The payload [`parse`](Self::parse) reads, ordered by the first offset so
    /// the map reads as a partition of the segment, and column-aligned.
    pub fn to_text(&self) -> String {
        let mut rows: Vec<(&str, &Vec<DataRange>)> =
            self.entries.iter().map(|(n, r)| (n.as_str(), r)).collect();
        rows.sort_by_key(|(name, ranges)| (ranges[0], *name));
        let width = rows.iter().map(|(name, _)| name.len()).max().unwrap_or(0);

        let mut text = String::new();
        for (name, ranges) in rows {
            write!(text, "{name:width$}").expect("writing to a String cannot fail");
            for range in ranges {
                write!(text, " {}:{}+{}", range.segment, range.offset, range.size)
                    .expect("writing to a String cannot fail");
            }
            text.push('\n');
        }
        text
    }
}

/// Sort-then-merge the ranges of each segment, absorbing gaps up to `gap`: a
/// second segment costs more header than a short gap costs payload.
pub(crate) fn merge_with_gap(ranges: &mut Vec<DataRange>, gap: u32) {
    ranges.sort_unstable();
    ranges.dedup_by(|next, last| {
        let adjoins = last.segment == next.segment && next.offset <= last.end().saturating_add(gap);
        if adjoins {
            last.size = next.end().max(last.end()) - last.offset;
        }
        adjoins
    });
}

fn merge(ranges: &mut Vec<DataRange>) {
    merge_with_gap(ranges, 0);
}

/// Resolve the map from a module wasm-ld linked with `--emit-relocs`.
///
/// Only `reloc.CODE` is read: an entry there names a data symbol, and the byte
/// it patches locates the function that names it. A relocation in the data
/// section would mean a pointer stored in the data itself — a data-to-data or
/// data-to-function edge this map cannot express — so one is an error rather
/// than something to resolve halfway.
pub fn resolve(wasm: &[u8]) -> Result<DataRefs, Error> {
    use wasmparser::{Payload, RelocSectionReader};

    // A relocation names its target by position among the module's sections,
    // custom ones counted, so the walk has to count them the same way. A code
    // section arrives as a header followed by one payload per function; only
    // the header is a section.
    let mut sections = 0;
    let mut code_section = None;
    let mut data_section = None;
    let mut code_range = 0..0;
    let mut bodies: Vec<std::ops::Range<usize>> = Vec::new();
    let mut imported_funcs = 0;
    let mut linking = None;
    let mut relocs = Vec::new();
    let mut names: BTreeMap<u32, &str> = BTreeMap::new();

    for payload in wasmparser::Parser::new(0).parse_all(wasm) {
        let payload = payload?;
        let index = sections;
        if payload.as_section().is_some() {
            sections += 1;
        }
        match payload {
            Payload::ImportSection(reader) => {
                for import in reader.into_imports() {
                    if matches!(
                        import?.ty,
                        wasmparser::TypeRef::Func(_) | wasmparser::TypeRef::FuncExact(_)
                    ) {
                        imported_funcs += 1;
                    }
                }
            }
            Payload::CodeSectionStart { range, .. } => {
                code_section = Some(index);
                code_range = range;
            }
            Payload::CodeSectionEntry(body) => bodies.push(body.range()),
            Payload::DataSection(_) => data_section = Some(index),
            Payload::CustomSection(reader) if reader.name() == "linking" => {
                linking = Some((reader.data(), reader.data_offset()));
            }
            Payload::CustomSection(reader) if reader.name().starts_with("reloc.") => {
                relocs.push((reader.data(), reader.data_offset()));
            }
            Payload::CustomSection(reader) if reader.name() == "name" => {
                let section = wasmparser::NameSectionReader::new(wasmparser::BinaryReader::new(
                    reader.data(),
                    reader.data_offset(),
                ));
                for subsection in section {
                    if let wasmparser::Name::Function(map) = subsection? {
                        for naming in map {
                            let naming = naming?;
                            names.insert(naming.index, naming.name);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let (data, offset) = linking.ok_or_else(|| {
        Error::DataRef("no `linking` section: link the asset with `--emit-relocs`".into())
    })?;
    let symbols = data_symbols(data, offset)?;

    let mut entries: BTreeMap<String, Vec<DataRange>> = BTreeMap::new();
    for (data, offset) in relocs {
        let reader = RelocSectionReader::new(wasmparser::BinaryReader::new(data, offset))?;
        if Some(reader.section_index()) == data_section {
            return Err(Error::DataRef(
                "`reloc.DATA` names a pointer stored in the data itself, which the \
                 function-to-data map cannot express"
                    .into(),
            ));
        }
        if Some(reader.section_index()) != code_section {
            continue;
        }
        for entry in reader.entries() {
            let entry = entry?;
            let Some(range) = symbols.get(&entry.index) else {
                continue;
            };
            let position = code_range.start + entry.offset as usize;
            let body = bodies
                .iter()
                .position(|body| body.contains(&position))
                .ok_or_else(|| {
                    Error::DataRef(format!(
                        "relocation at code offset {} falls outside every function body",
                        entry.offset
                    ))
                })?;
            let func = imported_funcs + body as u32;
            let name = names.get(&func).ok_or_else(|| {
                Error::DataRef(format!(
                    "function {func} reads data but the `name` section does not name it"
                ))
            })?;
            entries.entry((*name).to_string()).or_default().push(*range);
        }
    }
    for ranges in entries.values_mut() {
        merge(ranges);
    }
    Ok(DataRefs { entries })
}

/// The range each data symbol of the `linking` section defines, by symbol
/// index. A relocation names one of these.
fn data_symbols(data: &[u8], offset: usize) -> Result<BTreeMap<u32, DataRange>, Error> {
    use wasmparser::{Linking, LinkingSectionReader, SymbolInfo};

    let mut symbols = BTreeMap::new();
    let reader = LinkingSectionReader::new(wasmparser::BinaryReader::new(data, offset))?;
    for subsection in reader.subsections() {
        let Linking::SymbolTable(map) = subsection? else {
            continue;
        };
        for (index, symbol) in map.into_iter().enumerate() {
            let SymbolInfo::Data {
                symbol: Some(defined),
                ..
            } = symbol?
            else {
                continue;
            };
            if defined.size == 0 {
                continue;
            }
            let range =
                DataRange::new(defined.index, defined.offset, defined.size).ok_or_else(|| {
                    Error::DataRef(format!(
                        "symbol {index} spans {} bytes from {} of segment {}, which the \
                         address space cannot hold",
                        defined.size, defined.offset, defined.index
                    ))
                })?;
            symbols.insert(index as u32, range);
        }
    }
    Ok(symbols)
}
