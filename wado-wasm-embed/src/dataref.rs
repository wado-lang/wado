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
    /// `None` for a range naming no bytes, or with no end to reach. Every
    /// constructor goes through here, so [`end`](Self::end) is total and a
    /// surviving range always keeps something.
    fn new(segment: u32, offset: u32, size: u32) -> Option<Self> {
        if size == 0 {
            return None;
        }
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

/// What one edge of the map reaches.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    Data(DataRange),
    Func(String),
}

/// A pointer stored in the data itself, at `segment:offset`, and what it reaches.
/// Keeping the bytes it occupies without keeping its target would leave live
/// code dereferencing a range the prune took away.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Pointer {
    pub segment: u32,
    pub offset: u32,
    pub target: Target,
}

/// What a function reads, and what the data itself points at. Both are keyed by
/// the asset's own names and segment indices, so a map and the module it
/// describes only mean anything together.
#[derive(Debug)]
pub struct DataRefs {
    entries: BTreeMap<String, Vec<DataRange>>,
    pointers: Vec<Pointer>,
}

impl DataRefs {
    pub fn get(&self, func_name: &str) -> Option<&[DataRange]> {
        self.entries.get(func_name).map(Vec::as_slice)
    }

    pub fn pointers(&self) -> &[Pointer] {
        &self.pointers
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.pointers.is_empty()
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

    /// Parse the payload of a [`SECTION_NAME`] section. A line is either what a
    /// function reads, `<name> <segment>:<offset>+<size> ...`, or where a
    /// pointer sits and what it reaches, `@<segment>:<offset> <target>`. Fields
    /// are separated by any run of spaces so the emitted form can align them.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut entries = BTreeMap::new();
        let mut pointers = Vec::new();
        for (number, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let malformed = || Error::DataRef(format!("line {}: malformed: {line}", number + 1));
            let mut fields = line.split_ascii_whitespace();
            let head = fields.next().ok_or_else(malformed)?;
            if let Some(site) = head.strip_prefix('@') {
                let (segment, offset) = site.split_once(':').ok_or_else(malformed)?;
                let target = fields.next().ok_or_else(malformed)?;
                if fields.next().is_some() {
                    return Err(malformed());
                }
                pointers.push(Pointer {
                    segment: segment.parse().map_err(|_| malformed())?,
                    offset: offset.parse().map_err(|_| malformed())?,
                    target: parse_target(target).ok_or_else(malformed)?,
                });
                continue;
            }
            let mut ranges = Vec::new();
            for field in fields {
                ranges.push(parse_range(field).ok_or_else(malformed)?);
            }
            if ranges.is_empty() {
                return Err(malformed());
            }
            merge(&mut ranges);
            if entries.insert(head.to_string(), ranges).is_some() {
                return Err(Error::DataRef(format!("`{head}` is listed twice")));
            }
        }
        // Indistinguishable from a map that failed to resolve, and honouring it
        // prunes every data segment away. An asset with nothing to say carries
        // no section at all.
        if entries.is_empty() && pointers.is_empty() {
            return Err(Error::DataRef("names no function".into()));
        }
        pointers.sort_by_key(|p| (p.segment, p.offset));
        pointers.dedup();
        Ok(DataRefs { entries, pointers })
    }

    /// The payload [`parse`](Self::parse) reads, ordered by the first offset so
    /// the map reads as a partition of the segment, and column-aligned. The
    /// pointer lines follow, in address order.
    pub fn to_text(&self) -> String {
        let mut rows: Vec<(&str, &Vec<DataRange>)> =
            self.entries.iter().map(|(n, r)| (n.as_str(), r)).collect();
        rows.sort_by_key(|(name, ranges)| (ranges[0], *name));
        let sites: Vec<String> = self
            .pointers
            .iter()
            .map(|p| format!("@{}:{}", p.segment, p.offset))
            .collect();
        // Aligning to the longest name pads every line to it, and a mangled
        // Rust symbol runs past a hundred characters. Past the cap a name gets
        // one space, which costs the reader less than the padding does.
        const ALIGN_CAP: usize = 40;
        let width = rows
            .iter()
            .map(|(name, _)| name.len())
            .chain(sites.iter().map(String::len))
            .filter(|len| *len <= ALIGN_CAP)
            .max()
            .unwrap_or(0);

        let mut text = String::new();
        for (name, ranges) in rows {
            write!(text, "{name:width$}").expect("writing to a String cannot fail");
            for range in ranges {
                write!(text, " {}:{}+{}", range.segment, range.offset, range.size)
                    .expect("writing to a String cannot fail");
            }
            text.push('\n');
        }
        for (site, pointer) in sites.iter().zip(&self.pointers) {
            let target = match &pointer.target {
                Target::Data(r) => format!("{}:{}+{}", r.segment, r.offset, r.size),
                Target::Func(name) => name.clone(),
            };
            writeln!(text, "{site:width$} {target}").expect("writing to a String cannot fail");
        }
        text
    }
}

fn parse_range(field: &str) -> Option<DataRange> {
    let (segment, rest) = field.split_once(':')?;
    let (offset, size) = rest.split_once('+')?;
    DataRange::new(
        segment.parse().ok()?,
        offset.parse().ok()?,
        size.parse().ok()?,
    )
}

/// A target spelling a range is one; anything else names a function. A `:` is
/// what tells them apart, and no symbol name carries one.
fn parse_target(field: &str) -> Option<Target> {
    if field.contains(':') {
        return parse_range(field).map(Target::Data);
    }
    Some(Target::Func(field.to_string()))
}

/// Sort-then-merge the ranges of each segment, absorbing gaps up to `gap`: a
/// second segment costs more header than a short gap costs payload.
pub(crate) fn merge_with_gap(ranges: &mut Vec<DataRange>, gap: u32) {
    ranges.sort();
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
/// A `reloc.CODE` entry names a data symbol, and the byte it patches locates the
/// function that names it. A `reloc.DATA` entry is a pointer stored in the data
/// itself: the byte it patches locates the segment and offset, and the symbol it
/// names is what the pointer reaches — another data range, or a function.
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
    let mut data_range = 0..0;
    let mut bodies: Vec<std::ops::Range<usize>> = Vec::new();
    let mut segments: Vec<std::ops::Range<usize>> = Vec::new();
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
            Payload::DataSection(reader) => {
                data_section = Some(index);
                data_range = reader.range();
                for data in reader {
                    let data = data?;
                    // The payload's own bytes, which is what an offset into the
                    // section lands in.
                    segments.push(data.range.end - data.data.len()..data.range.end);
                }
            }
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
    let symbols = symbol_targets(data, offset, &names)?;

    let mut entries: BTreeMap<String, Vec<DataRange>> = BTreeMap::new();
    let mut pointers = Vec::new();
    for (data, offset) in relocs {
        let reader = RelocSectionReader::new(wasmparser::BinaryReader::new(data, offset))?;
        let section = Some(reader.section_index());
        if section != code_section && section != data_section {
            continue;
        }
        for entry in reader.entries() {
            let entry = entry?;
            let Some(target) = symbols.get(&entry.index) else {
                continue;
            };
            if section == data_section {
                let position = data_range.start + entry.offset as usize;
                let (segment, payload) = segments
                    .iter()
                    .enumerate()
                    .find(|(_, payload)| payload.contains(&position))
                    .ok_or_else(|| {
                        Error::DataRef(format!(
                            "relocation at data offset {} falls outside every segment",
                            entry.offset
                        ))
                    })?;
                pointers.push(Pointer {
                    segment: segment as u32,
                    offset: (position - payload.start) as u32,
                    target: target.clone(),
                });
                continue;
            }
            // Only a data target says which bytes a function reads; a call
            // relocation is an edge the code walk already follows.
            let Target::Data(range) = target else {
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
    pointers.sort_by_key(|p| (p.segment, p.offset));
    pointers.dedup();
    Ok(DataRefs { entries, pointers })
}

/// What each symbol of the `linking` section reaches, by symbol index: the range
/// a data symbol defines, or the name of a function symbol. A relocation names
/// one of these.
fn symbol_targets(
    data: &[u8],
    offset: usize,
    names: &BTreeMap<u32, &str>,
) -> Result<BTreeMap<u32, Target>, Error> {
    use wasmparser::{Linking, LinkingSectionReader, SymbolInfo};

    let mut symbols = BTreeMap::new();
    let reader = LinkingSectionReader::new(wasmparser::BinaryReader::new(data, offset))?;
    for subsection in reader.subsections() {
        let Linking::SymbolTable(map) = subsection? else {
            continue;
        };
        for (index, symbol) in map.into_iter().enumerate() {
            let target = match symbol? {
                SymbolInfo::Data {
                    symbol: Some(defined),
                    ..
                } => {
                    if defined.size == 0 {
                        continue;
                    }
                    Target::Data(
                        DataRange::new(defined.index, defined.offset, defined.size).ok_or_else(
                            || {
                                Error::DataRef(format!(
                                    "symbol {index} spans {} bytes from {} of segment {}, which \
                                     the address space cannot hold",
                                    defined.size, defined.offset, defined.index
                                ))
                            },
                        )?,
                    )
                }
                // A function symbol is named by the `name` section, which is
                // what the emitted map keys functions by. One the section does
                // not name is one no pointer can be checked against.
                SymbolInfo::Func { index: func, .. } => match names.get(&func) {
                    Some(name) => Target::Func((*name).to_string()),
                    None => continue,
                },
                _ => continue,
            };
            symbols.insert(index as u32, target);
        }
    }
    Ok(symbols)
}
