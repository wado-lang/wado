//! `rust-move`: moves Rust items to another file.
//!
//! rust-analyzer has no such refactoring, so the move is this module's. The
//! server answers what text cannot: where an item starts and ends, who refers
//! to it, and what the names in its body resolve to. From those the move keeps
//! the code compiling across the file boundary — it widens a visibility the
//! boundary now crosses and repeats the imports the moved code reads.

mod text;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process;

use lexopt::Arg::{Long, Value as Positional};
use serde_json::{Value, json};

use crate::rust_rename::edit::{self, Position};
use crate::rust_rename::lsp::{Client, file_uri, uri_path};
use crate::rust_rename::symbol::Location;
use crate::rust_rename::{Locator, Mode, await_workspace, display_path, start_server};
use text::{UseItem, identifiers, is_keyword, use_items};

/// `SymbolKind` values of the LSP specification.
const MODULE_KIND: u64 = 2;
const FIELD_KIND: u64 = 8;
const ENUM_MEMBER_KIND: u64 = 22;
const TYPE_PARAMETER_KIND: u64 = 26;

const USAGE: &str = "usage: rust-move [--workspace DIR] [--dry-run] --to FILE LOCATOR...";

pub fn run(mut parser: lexopt::Parser) {
    let mut workspace = PathBuf::from(".");
    let mut mode = Mode::Apply;
    let mut to = None;
    let mut locators = Vec::new();
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Long("workspace") => workspace = parser.value().unwrap().into(),
            Long("dry-run") => mode = Mode::DryRun,
            Long("to") => to = Some(PathBuf::from(parser.value().unwrap())),
            Positional(value) => locators.push(value.to_string_lossy().into_owned()),
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }
    let to = to.unwrap_or_else(|| panic!("{USAGE}"));
    assert!(!locators.is_empty(), "{USAGE}");
    let result = locators
        .iter()
        .map(|text| Locator::parse(text))
        .collect::<Result<Vec<_>, _>>()
        .and_then(|locators| move_items(&workspace, &locators, &to, mode));
    match result {
        Ok(report) => {
            for line in report {
                println!("{line}");
            }
        }
        Err(message) => {
            eprintln!("error: {message}");
            process::exit(1);
        }
    }
}

/// Moves the items `locators` name into the file `to`, and returns what it
/// did, or with `Mode::DryRun` what it would do, one line per step.
pub fn move_items(
    workspace: &Path,
    locators: &[Locator],
    to: &Path,
    mode: Mode,
) -> Result<Vec<String>, String> {
    let root = workspace
        .canonicalize()
        .map_err(|e| format!("{}: {e}", workspace.display()))?;
    let to = to
        .canonicalize()
        .map_err(|e| format!("{}: {e}", to.display()))?;
    let mut client = start_server(&root);
    let result = await_workspace(&mut client).and_then(|()| plan(&mut client, locators, &to));
    client.shutdown();
    let plan = result?;
    if mode == Mode::Apply {
        for (path, text) in &plan.files {
            fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    Ok(plan.report)
}

/// The new text of every file the move edits, and the account of it.
struct Plan {
    files: BTreeMap<PathBuf, String>,
    report: Vec<String>,
}

/// A file as the server and the move both read it.
struct Document {
    path: PathBuf,
    uri: String,
    text: String,
    lines: Vec<String>,
    /// Its `DocumentSymbol` tree.
    symbols: Vec<Value>,
    /// Every identifier it writes, in order.
    identifiers: Vec<(Position, String)>,
}

impl Document {
    fn load(client: &mut Client, path: &Path) -> Result<Document, String> {
        let uri = file_uri(path);
        let symbols = client.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        )?;
        let text = edit::read(path);
        assert!(
            !text.contains('\r'),
            "a source file ends its lines with `\\n`"
        );
        let line_starts: Vec<usize> = [0]
            .into_iter()
            .chain(text.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        let identifiers = identifiers(&text)
            .into_iter()
            .map(|(at, name)| {
                let line = line_starts.partition_point(|&start| start <= at) - 1;
                let position = Position {
                    line,
                    column: at - line_starts[line],
                };
                (position, name.to_string())
            })
            .collect();
        Ok(Document {
            path: path.to_path_buf(),
            uri,
            lines: text.lines().map(str::to_string).collect(),
            identifiers,
            text,
            symbols: symbols.as_array().cloned().unwrap_or_default(),
        })
    }

    /// The identifiers `item` and the header of its block write.
    fn mentions<'a>(&'a self, item: &'a Moved) -> impl Iterator<Item = &'a (Position, String)> {
        self.identifiers.iter().filter(|(position, _)| {
            item.span.contains(&position.line)
                || item
                    .header_span
                    .as_ref()
                    .is_some_and(|header| header.contains(&position.line))
        })
    }

    fn offset(&self, position: Position) -> usize {
        edit::offset(&self.text, position).expect("the server positions symbols on the text")
    }

    /// The file's name as a module, `None` for one that names its directory.
    fn module_name(&self) -> Option<&str> {
        let stem = self.path.file_stem()?.to_str()?;
        (!matches!(stem, "mod" | "lib" | "main")).then_some(stem)
    }

    /// The names its top level already gives a meaning.
    fn bound_names(&self, uses: &[UseItem]) -> BTreeSet<String> {
        uses.iter()
            .flat_map(|item| item.bindings.iter().map(|b| b.name.clone()))
            .chain(self.symbols.iter().map(|s| symbol_name(s).to_string()))
            .collect()
    }
}

fn symbol_name(symbol: &Value) -> &str {
    symbol["name"]
        .as_str()
        .expect("a document symbol has a name")
}

fn name_position(symbol: &Value) -> Position {
    Position::from_lsp(&symbol["selectionRange"]["start"])
}

/// The whole lines `symbol` spans, its attributes and doc comment included.
fn line_span(symbol: &Value) -> Range<usize> {
    let start = Position::from_lsp(&symbol["range"]["start"]);
    let end = Position::from_lsp(&symbol["range"]["end"]);
    start.line..end.line + 1
}

fn children(symbol: &Value) -> &[Value] {
    symbol["children"].as_array().map_or(&[], Vec::as_slice)
}

fn is_impl(symbol: &Value) -> bool {
    symbol_name(symbol).starts_with("impl")
}

fn is_trait_impl(symbol: &Value) -> bool {
    is_impl(symbol) && symbol_name(symbol).contains(" for ")
}

/// The symbol named at `position`, and the symbol holding it.
fn find_symbol<'a>(
    symbols: &'a [Value],
    position: Position,
    parent: Option<&'a Value>,
) -> Option<(&'a Value, Option<&'a Value>)> {
    symbols.iter().find_map(|symbol| {
        if name_position(symbol) == position {
            return Some((symbol, parent));
        }
        find_symbol(children(symbol), position, Some(symbol))
    })
}

/// Where an `impl` block's text starts and where its opening brace is.
fn impl_header_range(document: &Document, block: &Value) -> Range<usize> {
    let start = document.offset(Position::from_lsp(&block["range"]["start"]));
    let name_at = document.offset(name_position(block));
    let brace = name_at
        + document.text[name_at..]
            .find('{')
            .expect("an impl block opens a brace");
    start..brace
}

/// An `impl` block's text up to its opening brace, its comments left out: what
/// two blocks share when one's items may move into the other.
fn impl_header(document: &Document, block: &Value) -> String {
    document.text[impl_header_range(document, block)]
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn normalized(header: &str) -> String {
    header.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One item the move takes.
struct Moved {
    source: PathBuf,
    span: Range<usize>,
    name: String,
    name_position: Position,
    /// The header of the `impl` block it is a member of.
    header: Option<String>,
    /// The lines that header is written on.
    header_span: Option<Range<usize>>,
    /// The span of that block, where every member of it moves.
    emptied_block: Option<Range<usize>>,
}

/// A moved item's text, under the header of the `impl` block it is a member
/// of.
type Taken = (Option<String>, Vec<String>);

/// A visibility to write ahead of the declaration on `line`.
struct Widening {
    line: usize,
    visibility: &'static str,
}

/// Whether the declaration on `line` already writes a visibility. rustfmt keeps
/// one on the line of the name it qualifies.
fn declares_visibility(line: &str) -> bool {
    line.trim_start().starts_with("pub")
}

/// The narrowest visibility that lets every file of `users` read an item
/// declared in `home`.
fn visibility_reaching<'a>(
    home: &Path,
    users: impl IntoIterator<Item = &'a PathBuf>,
) -> &'static str {
    let parent = home.parent().expect("a source file has a directory");
    let names_its_directory = matches!(
        home.file_stem().and_then(|stem| stem.to_str()),
        Some("mod" | "lib" | "main")
    );
    let within_parent_module =
        |user: &PathBuf| user.starts_with(parent) || **user == parent.with_extension("rs");
    if !names_its_directory && users.into_iter().all(within_parent_module) {
        "pub(super)"
    } else {
        "pub(crate)"
    }
}

fn are_siblings(a: &Document, b: &Document) -> bool {
    a.path.parent() == b.path.parent() && a.module_name().is_some() && b.module_name().is_some()
}

fn locations(result: &Value) -> Vec<Location> {
    let one = |value: &Value| {
        // A `LocationLink` names its target; a `Location` is one.
        let (uri, range) = match value.get("targetUri") {
            Some(uri) => (uri, &value["targetSelectionRange"]),
            None => (&value["uri"], &value["range"]),
        };
        Some(Location {
            uri: uri.as_str()?.to_string(),
            position: Position::from_lsp(&range["start"]),
        })
    };
    match result {
        Value::Array(many) => many.iter().filter_map(one).collect(),
        Value::Null => Vec::new(),
        single => one(single).into_iter().collect(),
    }
}

fn plan(client: &mut Client, locators: &[Locator], to: &Path) -> Result<Plan, String> {
    let destination = Document::load(client, to)?;
    let mut sources: BTreeMap<PathBuf, Document> = BTreeMap::new();
    let mut moved: Vec<Moved> = Vec::new();
    for locator in locators {
        let target = locator.resolve(client)?;
        let path = uri_path(&target.uri);
        if path == destination.path {
            return Err(format!(
                "{} is already in {}",
                display_position(&path, target.position),
                display_path(&destination.path)
            ));
        }
        if !sources.contains_key(&path) {
            sources.insert(path.clone(), Document::load(client, &path)?);
        }
        moved.push(moved_item(&sources[&path], target.position)?);
    }
    for (index, item) in moved.iter().enumerate() {
        if moved[..index]
            .iter()
            .any(|other| other.source == item.source && other.span == item.span)
        {
            return Err(format!("`{}` is named twice", item.name));
        }
    }
    mark_emptied_blocks(&sources, &mut moved);

    let is_moved = |path: &Path, line: usize| {
        moved
            .iter()
            .any(|item| item.source == path && item.span.contains(&line))
    };
    let mut report = Vec::new();
    let mut widenings: BTreeMap<PathBuf, Vec<Widening>> = BTreeMap::new();
    // By the name each binds, so two sources importing one name by two paths
    // leave the destination one import.
    let mut imports: BTreeMap<PathBuf, BTreeMap<String, String>> = BTreeMap::new();
    let destination_uses = use_items(&destination.text);
    // A top-level item that moves is the destination's own from here on.
    let destination_names: BTreeSet<String> = destination
        .bound_names(&destination_uses)
        .into_iter()
        .chain(
            moved
                .iter()
                .filter(|item| item.header.is_none())
                .map(|item| item.name.clone()),
        )
        .collect();

    // An item the move takes out of the reach of who refers to it.
    for item in &moved {
        let source = &sources[&item.source];
        let references = client.request(
            "textDocument/references",
            json!({
                "textDocument": { "uri": source.uri },
                "position": item.name_position.to_lsp(),
                "context": { "includeDeclaration": false },
            }),
        )?;
        let users: BTreeSet<PathBuf> = locations(&references)
            .into_iter()
            .map(|reference| (uri_path(&reference.uri), reference.position.line))
            .filter(|(path, line)| *path != destination.path && !is_moved(path, *line))
            .map(|(path, _)| path)
            .collect();
        if users.is_empty() {
            continue;
        }
        if !declares_visibility(&source.lines[item.name_position.line]) {
            widenings
                .entry(item.source.clone())
                .or_default()
                .push(Widening {
                    line: item.name_position.line,
                    visibility: visibility_reaching(&destination.path, &users),
                });
        }
        // A member is reached through its type, wherever its block is; a free
        // item is reached by a path, which names the file it left.
        if item.header.is_none() {
            let mut by_hand = Vec::new();
            for user in &users {
                if *user == item.source && are_siblings(source, &destination) {
                    imports.entry(item.source.clone()).or_default().insert(
                        item.name.clone(),
                        format!(
                            "use super::{}::{};",
                            destination.module_name().expect("siblings name modules"),
                            item.name
                        ),
                    );
                } else {
                    by_hand.push(display_path(user));
                }
            }
            if !by_hand.is_empty() {
                report.push(format!(
                    "by hand: `{}` is named in {}",
                    item.name,
                    by_hand.join(", ")
                ));
            }
        }
    }

    // What the moved code reads of the file it leaves: a private item there
    // is now out of reach, and a free one needs an import.
    for item in &moved {
        let source = &sources[&item.source];
        let mut helpers: BTreeSet<(usize, usize)> = BTreeSet::new();
        for (position, name) in source.mentions(item) {
            if is_keyword(name) {
                continue;
            }
            let definition = client.request(
                "textDocument/definition",
                json!({
                    "textDocument": { "uri": source.uri },
                    "position": position.to_lsp(),
                }),
            )?;
            for target in locations(&definition) {
                if target.uri == source.uri && !is_moved(&source.path, target.position.line) {
                    helpers.insert((target.position.line, target.position.column));
                }
            }
        }
        for (line, column) in helpers {
            let position = Position { line, column };
            let Some((helper, parent)) = find_symbol(&source.symbols, position, None) else {
                continue;
            };
            let kind = helper["kind"].as_u64();
            if matches!(kind, Some(ENUM_MEMBER_KIND | TYPE_PARAMETER_KIND)) {
                continue;
            }
            let name = symbol_name(helper);
            match parent {
                // A trait's member is as visible as the trait.
                Some(parent)
                    if is_trait_impl(parent) || (!is_impl(parent) && kind != Some(FIELD_KIND)) =>
                {
                    if parent["kind"].as_u64() == Some(MODULE_KIND) {
                        report.push(format!(
                            "by hand: `{name}` in the module `{}` of {} is read by the moved code",
                            symbol_name(parent),
                            display_path(&source.path)
                        ));
                    }
                    continue;
                }
                Some(_) => {}
                None if are_siblings(source, &destination) => {
                    if !destination_names.contains(name) {
                        imports.entry(destination.path.clone()).or_default().insert(
                            name.to_string(),
                            format!(
                                "use super::{}::{name};",
                                source.module_name().expect("siblings name modules")
                            ),
                        );
                    }
                }
                None => report.push(format!(
                    "by hand: import `{name}` of {} into {}",
                    display_path(&source.path),
                    display_path(&destination.path)
                )),
            }
            let widened = widenings.entry(item.source.clone()).or_default();
            if !declares_visibility(&source.lines[line]) && !widened.iter().any(|w| w.line == line)
            {
                widened.push(Widening {
                    line,
                    visibility: visibility_reaching(&source.path, [&destination.path]),
                });
            }
        }
    }

    // The imports the moved code reads by name.
    for source in sources.values() {
        let mentioned: BTreeSet<String> = moved
            .iter()
            .filter(|item| item.source == source.path)
            .flat_map(|item| source.mentions(item))
            .map(|(_, name)| name.clone())
            .collect();
        for item in use_items(&source.text).iter().filter(|item| item.plain) {
            for binding in &item.bindings {
                if !mentioned.contains(&binding.name) || destination_names.contains(&binding.name) {
                    continue;
                }
                if binding.is_relative() && !are_siblings(source, &destination) {
                    report.push(format!(
                        "by hand: `{}` of {} is relative to it",
                        binding.item(),
                        display_path(&source.path)
                    ));
                    continue;
                }
                imports
                    .entry(destination.path.clone())
                    .or_default()
                    .entry(binding.name.clone())
                    .or_insert_with(|| binding.item());
            }
            for glob in &item.globs {
                if !destination_uses.iter().any(|d| d.globs.contains(glob)) {
                    report.push(format!(
                        "by hand: the moved code may read `use {glob}::*;` of {}",
                        display_path(&source.path)
                    ));
                }
            }
        }
    }

    for (path, widened) in &widenings {
        for widening in widened {
            report.push(format!(
                "widen {}:{} to {}",
                display_path(path),
                widening.line + 1,
                widening.visibility
            ));
        }
    }
    let imports: BTreeMap<PathBuf, BTreeSet<String>> = imports
        .into_iter()
        .map(|(path, by_name)| (path, by_name.into_values().collect()))
        .collect();
    for (path, items) in &imports {
        for item in items {
            report.push(format!("import in {}: {item}", display_path(path)));
        }
    }
    for item in &moved {
        report.push(format!(
            "move {}:{} `{}` to {}",
            display_path(&item.source),
            item.span.start + 1,
            item.name,
            display_path(&destination.path)
        ));
    }
    report.push(
        "next: `cargo check` names a trait import the moved code calls a method of, \
         and `cargo clippy --fix` removes the imports left unused"
            .to_string(),
    );

    // Every position above is in the text as read, so the lines are edited in
    // place first and cut or spliced after.
    let mut files = BTreeMap::new();
    let mut taken: Vec<Option<Taken>> = moved.iter().map(|_| None).collect();
    for source in sources.values() {
        let mut lines = source.lines.clone();
        for widening in widenings.get(&source.path).into_iter().flatten() {
            let line = &mut lines[widening.line];
            let indent = line.len() - line.trim_start().len();
            line.insert_str(indent, &format!("{} ", widening.visibility));
        }
        let mut cut: Vec<Range<usize>> = Vec::new();
        let own = moved
            .iter()
            .enumerate()
            .filter(|(_, item)| item.source == source.path);
        for (index, item) in own {
            taken[index] = Some((item.header.clone(), lines[item.span.clone()].to_vec()));
            let span = item.emptied_block.clone().unwrap_or(item.span.clone());
            if !cut.contains(&span) {
                cut.push(span);
            }
        }
        let own_imports = imports.get(&source.path).cloned().unwrap_or_default();
        files.insert(
            source.path.clone(),
            render(without(lines, &cut), &own_imports, &[], source),
        );
    }
    let taken: Vec<Taken> = taken
        .into_iter()
        .map(|item| item.expect("every moved item has a source"))
        .collect();
    let own_imports = imports.get(&destination.path).cloned().unwrap_or_default();
    files.insert(
        destination.path.clone(),
        render(
            destination.lines.clone(),
            &own_imports,
            &taken,
            &destination,
        ),
    );
    Ok(Plan { files, report })
}

fn display_position(path: &Path, position: Position) -> String {
    format!(
        "{}:{}:{}",
        display_path(path),
        position.line + 1,
        position.column + 1
    )
}

fn moved_item(source: &Document, position: Position) -> Result<Moved, String> {
    let at = display_position(&source.path, position);
    let (symbol, parent) = find_symbol(&source.symbols, position, None)
        .ok_or_else(|| format!("{at} names no item"))?;
    let block = match parent {
        None => None,
        Some(block) if is_trait_impl(block) => {
            return Err(format!(
                "{at} is a member of `{}`, which it cannot leave",
                symbol_name(block)
            ));
        }
        Some(block) if is_impl(block) => Some(block),
        Some(other) => {
            return Err(format!(
                "{at} is inside `{}`; only a top-level item or a member of an inherent impl moves",
                symbol_name(other)
            ));
        }
    };
    let span = line_span(symbol);
    let start = Position::from_lsp(&symbol["range"]["start"]);
    let end = Position::from_lsp(&symbol["range"]["end"]);
    let alone = source.lines[start.line][..start.column].trim().is_empty()
        && source.lines[end.line][end.column..].trim().is_empty();
    if !alone {
        return Err(format!("{at} shares its lines with other code"));
    }
    Ok(Moved {
        source: source.path.clone(),
        span,
        name: symbol_name(symbol).to_string(),
        name_position: position,
        header: block.map(|block| impl_header(source, block)),
        header_span: block.map(|block| {
            let range = impl_header_range(source, block);
            let line_of = |at: usize| source.text[..at].matches('\n').count();
            line_of(range.start)..line_of(range.end) + 1
        }),
        emptied_block: None,
    })
}

/// Marks the members of each `impl` block the move takes every member of, so
/// the block goes with them.
fn mark_emptied_blocks(sources: &BTreeMap<PathBuf, Document>, moved: &mut [Moved]) {
    for source in sources.values() {
        for block in source.symbols.iter().filter(|s| is_impl(s)) {
            let members = children(block);
            let all_move = !members.is_empty()
                && members.iter().all(|member| {
                    moved
                        .iter()
                        .any(|item| item.source == source.path && item.span == line_span(member))
                });
            if !all_move {
                continue;
            }
            let block_span = line_span(block);
            for item in moved
                .iter_mut()
                .filter(|item| item.source == source.path && block_span.contains(&item.span.start))
            {
                item.emptied_block = Some(block_span.clone());
            }
        }
    }
}

/// `lines` without the spans of `cut`, and without the blank line each leaves
/// doubled or at the edge of its block.
fn without(lines: Vec<String>, cut: &[Range<usize>]) -> Vec<String> {
    let removed = |line: usize| cut.iter().any(|span| span.contains(&line));
    let mut kept: Vec<String> = Vec::new();
    let mut after_cut = false;
    for (index, line) in lines.into_iter().enumerate() {
        if removed(index) {
            after_cut = true;
            continue;
        }
        if after_cut {
            let above_is_open = kept
                .last()
                .is_none_or(|above| above.trim().is_empty() || above.trim_end().ends_with('{'));
            if line.trim().is_empty() && above_is_open {
                continue;
            }
            if line.trim_start().starts_with('}')
                && kept.last().is_some_and(|l| l.trim().is_empty())
            {
                kept.pop();
            }
        }
        after_cut = false;
        kept.push(line);
    }
    kept
}

/// The file `document` becomes: `lines` with `imports` after its last `use`
/// item, and each of `taken` in the `impl` block of its header, opened at the
/// end of the file where `document` has none.
fn render(
    mut lines: Vec<String>,
    imports: &BTreeSet<String>,
    taken: &[Taken],
    document: &Document,
) -> String {
    assert!(
        taken.is_empty() || lines == document.lines,
        "items land on the lines the symbols were read on"
    );
    // Bottom up, so a splice leaves the lines above it where they were read.
    let mut splices: Vec<(usize, Vec<String>)> = Vec::new();
    let tail = document
        .symbols
        .last()
        .filter(|last| last["kind"].as_u64() == Some(MODULE_KIND) && symbol_name(last) == "tests")
        .map_or(lines.len(), |tests| line_span(tests).start);
    let mut appended: Vec<String> = Vec::new();
    let mut headers: Vec<&Option<String>> = Vec::new();
    for (header, _) in taken {
        if !headers.contains(&header) {
            headers.push(header);
        }
    }
    for header in headers {
        let mut body: Vec<String> = Vec::new();
        for (_, item) in taken.iter().filter(|(h, _)| h == header) {
            if !body.is_empty() {
                body.push(String::new());
            }
            body.extend(item.iter().cloned());
        }
        let block = header.as_ref().and_then(|header| {
            document.symbols.iter().rev().find(|symbol| {
                is_impl(symbol)
                    && normalized(&impl_header(document, symbol)) == normalized(header)
                    && line_span(symbol).len() > 1
            })
        });
        match (header, block) {
            (_, Some(block)) => {
                let close = line_span(block).end - 1;
                if !children(block).is_empty() {
                    body.insert(0, String::new());
                }
                splices.push((close, body));
            }
            (Some(header), None) => {
                appended.push(String::new());
                appended.extend(header.lines().map(str::to_string));
                appended
                    .last_mut()
                    .expect("a header has a line")
                    .push_str(" {");
                appended.extend(body);
                appended.push("}".to_string());
            }
            (None, None) => {
                appended.push(String::new());
                appended.extend(body);
            }
        }
    }
    if !appended.is_empty() {
        if tail < lines.len() {
            // Ahead of the tests module, which stays last.
            appended.remove(0);
            appended.push(String::new());
        }
        splices.push((tail, appended));
    }
    if !imports.is_empty() {
        let after_uses = use_items(&lines.join("\n"))
            .iter()
            .map(|item| item.lines.end)
            .next_back();
        let at = after_uses.unwrap_or_else(|| {
            lines
                .iter()
                .position(|line| !line.starts_with("//!"))
                .unwrap_or(lines.len())
        });
        let mut block: Vec<String> = imports.iter().cloned().collect();
        if after_uses.is_none() {
            block.insert(0, String::new());
            if lines.get(at).is_some_and(|below| !below.is_empty()) {
                block.push(String::new());
            }
        }
        // Ahead of what lands on the same line: the sort is stable.
        splices.insert(0, (at, block));
    }
    splices.sort_by_key(|(at, _)| *at);
    for (at, block) in splices.into_iter().rev() {
        lines.splice(at..at, block);
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::rust_rename::tests::{probe, read};

    const MAIN: &str = "mod shape;\nmod area;\n\nfn main() {\n    \
                        let s = shape::Shape { side: 2 };\n    println!(\"{}\", s.doubled());\n}\n";

    const SHAPE: &str = "\
use std::collections::BTreeSet;
use std::fmt::Write;

pub struct Shape {
    pub side: i32,
}

const SCALE: i32 = 2;

impl Shape {
    pub fn doubled(&self) -> i32 {
        self.area() * SCALE
    }

    /// The area, squared sides.
    #[inline]
    fn area(&self) -> i32 {
        let seen: BTreeSet<i32> = BTreeSet::new();
        self.side * self.side + helper(seen.len())
    }

    fn label(&self) -> String {
        let mut out = String::new();
        write!(out, \"{}\", self.side).unwrap();
        out
    }
}

fn helper(n: usize) -> i32 {
    n as i32
}
";

    const AREA: &str = "\
use super::shape::Shape;

impl Shape {
    pub fn perimeter(&self) -> i32 {
        self.side * 4
    }
}
";

    fn workspace(area: &str) -> TempDir {
        probe(&[
            ("src/main.rs", MAIN),
            ("src/shape.rs", SHAPE),
            ("src/area.rs", area),
        ])
    }

    fn locators(names: &[&str]) -> Vec<Locator> {
        names.iter().map(|n| Locator::parse(n).unwrap()).collect()
    }

    fn run(dir: &TempDir, names: &[&str], mode: Mode) -> Result<Vec<String>, String> {
        move_items(
            dir.path(),
            &locators(names),
            &dir.path().join("src/area.rs"),
            mode,
        )
    }

    #[test]
    fn a_member_moves_into_the_block_of_its_header() {
        let dir = workspace(AREA);
        run(&dir, &["probe::shape::Shape::area"], Mode::Apply).unwrap();
        assert_eq!(
            read(&dir, "src/area.rs"),
            "\
use super::shape::Shape;
use std::collections::BTreeSet;
use super::shape::helper;

impl Shape {
    pub fn perimeter(&self) -> i32 {
        self.side * 4
    }

    /// The area, squared sides.
    #[inline]
    pub(super) fn area(&self) -> i32 {
        let seen: BTreeSet<i32> = BTreeSet::new();
        self.side * self.side + helper(seen.len())
    }
}
"
        );
        let shape = read(&dir, "src/shape.rs");
        assert!(!shape.contains("fn area"));
        assert!(shape.contains("        self.area() * SCALE\n    }\n\n    fn label"));
        assert!(shape.contains("pub(super) fn helper(n: usize)"));
    }

    #[test]
    fn a_file_without_the_block_gets_one() {
        let dir = workspace("//! Areas.\n");
        run(
            &dir,
            &["probe::shape::Shape::area", "probe::shape::helper"],
            Mode::Apply,
        )
        .unwrap();
        assert_eq!(
            read(&dir, "src/area.rs"),
            "\
//! Areas.

use std::collections::BTreeSet;
use super::shape::Shape;

impl Shape {
    /// The area, squared sides.
    #[inline]
    pub(super) fn area(&self) -> i32 {
        let seen: BTreeSet<i32> = BTreeSet::new();
        self.side * self.side + helper(seen.len())
    }
}

fn helper(n: usize) -> i32 {
    n as i32
}
"
        );
        assert!(!read(&dir, "src/shape.rs").contains("helper"));
    }

    #[test]
    fn a_block_goes_with_its_last_member() {
        let dir = workspace(AREA);
        run(
            &dir,
            &[
                "probe::shape::Shape::doubled",
                "probe::shape::Shape::area",
                "probe::shape::Shape::label",
            ],
            Mode::Apply,
        )
        .unwrap();
        let shape = read(&dir, "src/shape.rs");
        assert!(!shape.contains("impl Shape"));
        assert!(shape.contains("pub(super) const SCALE: i32 = 2;\n\npub(super) fn helper"));
        let area = read(&dir, "src/area.rs");
        // `write!` calls a method of `Write`, which no name in the text says.
        assert!(!area.contains("use std::fmt::Write;"));
        assert!(area.contains("use super::shape::SCALE;"));
        assert!(area.find("fn doubled").unwrap() < area.find("fn area").unwrap());
    }

    #[test]
    fn a_dry_run_reports_and_writes_nothing() {
        let dir = workspace(AREA);
        let report = run(&dir, &["probe::shape::Shape::area"], Mode::DryRun).unwrap();
        assert_eq!(read(&dir, "src/shape.rs"), SHAPE);
        assert_eq!(read(&dir, "src/area.rs"), AREA);
        let report = report.join("\n");
        assert!(report.contains("to pub(super)"), "{report}");
        assert!(
            report.contains("use std::collections::BTreeSet;"),
            "{report}"
        );
        assert!(report.contains("`area`"), "{report}");
    }

    #[test]
    fn what_cannot_move_is_refused() {
        let dir = probe(&[
            (
                "src/main.rs",
                "mod a;\nmod b;\nfn main() { println!(\"{}\", a::S); }\n",
            ),
            (
                "src/a.rs",
                "use std::fmt;\npub struct S;\nimpl fmt::Display for S {\n    \
                 fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str(\"s\") }\n}\n",
            ),
            ("src/b.rs", ""),
        ]);
        let line = 4;
        let column = "    fn ".len() + 1;
        let position = format!("{}:{line}:{column}", dir.path().join("src/a.rs").display());
        let error = move_items(
            dir.path(),
            &locators(&[&position]),
            &dir.path().join("src/b.rs"),
            Mode::Apply,
        )
        .unwrap_err();
        assert!(error.contains("which it cannot leave"), "{error}");
    }
}
