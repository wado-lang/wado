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
use text::{Binding, UseItem, identifiers, is_keyword, use_items};

/// `SymbolKind` values of the LSP specification.
const MODULE_KIND: u64 = 2;
const FIELD_KIND: u64 = 8;
/// What the server reports an `impl` block as.
const OBJECT_KIND: u64 = 19;
const ENUM_MEMBER_KIND: u64 = 22;

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

    /// The declarations the name written at `position` resolves to.
    fn definitions(&self, client: &mut Client, position: Position) -> Result<Meaning, String> {
        let definition = client.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": self.uri },
                "position": position.to_lsp(),
            }),
        )?;
        Ok(locations(&definition)
            .into_iter()
            .map(|target| (target.uri, target.position.line, target.position.column))
            .collect())
    }

    /// What `binding` of one of its `use` items imports.
    fn imported(&self, client: &mut Client, binding: &Binding) -> Result<Meaning, String> {
        let before = &self.text[..binding.at];
        let position = Position {
            line: before.matches('\n').count(),
            column: binding.at - before.rfind('\n').map_or(0, |at| at + 1),
        };
        self.definitions(client, position)
    }
}

/// What a name means where it is written: each declaration it resolves to, by
/// file, line and column.
type Meaning = BTreeSet<(String, usize, usize)>;

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
    symbol["kind"].as_u64() == Some(OBJECT_KIND)
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

/// The line an `impl` block's opening brace is on.
fn impl_header_line(document: &Document, block: &Value) -> usize {
    let brace = impl_header_range(document, block).end;
    document.text[..brace].matches('\n').count()
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
    /// Its `SymbolKind`, which tells a type from a value of the same name.
    kind: Option<u64>,
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

/// The visibility the declaration on `line` writes (`pub`, `pub(super)`).
/// rustfmt keeps one on the line of the name it qualifies.
fn declared_visibility(line: &str) -> Option<&str> {
    let line = line.trim_start();
    let rest = line.strip_prefix("pub")?;
    match rest.strip_prefix('(') {
        Some(scope) => Some(&line[..="pub(".len() + scope.find(')')?]),
        None => rest.starts_with(' ').then_some("pub"),
    }
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
        // A file declares one free item of a name and a kind.
        let declared_alike = item.header.is_none()
            && (destination
                .symbols
                .iter()
                .any(|s| symbol_name(s) == item.name && s["kind"].as_u64() == item.kind)
                || moved[..index].iter().any(|other| {
                    other.header.is_none() && other.name == item.name && other.kind == item.kind
                }));
        if declared_alike {
            return Err(format!(
                "{} would declare `{}` twice",
                display_path(&destination.path),
                item.name
            ));
        }
    }
    mark_emptied_blocks(&sources, &mut moved);

    let the_move = Move {
        destination_uses: use_items(&destination.text),
        destination,
        sources,
        moved,
    };
    let mut fixes = Fixes::default();
    the_move.keep_moved_items_in_reach(client, &mut fixes)?;
    the_move.keep_source_items_in_reach(client, &mut fixes)?;
    the_move.repeat_imports(client, &mut fixes)?;
    Ok(the_move.write(fixes))
}

/// What crossing the file boundary asks for, gathered before any text changes:
/// every position in it is in the text as read.
#[derive(Default)]
struct Fixes {
    report: Vec<String>,
    widenings: BTreeMap<PathBuf, Vec<Widening>>,
    /// By file, then by the name each import binds, so two sources importing
    /// one name by two paths leave the destination one import.
    imports: BTreeMap<PathBuf, BTreeMap<String, (String, Meaning)>>,
}

impl Fixes {
    /// Widens the declaration on `line` of `document` to `visibility`. One
    /// that writes a visibility already keeps it, and is reported where it
    /// does not reach as far.
    fn widen(&mut self, document: &Document, line: usize, visibility: &'static str) {
        match declared_visibility(&document.lines[line]) {
            None => {
                let widened = self.widenings.entry(document.path.clone()).or_default();
                if !widened.iter().any(|w| w.line == line) {
                    widened.push(Widening { line, visibility });
                }
            }
            Some("pub" | "pub(crate)") => {}
            Some(declared) if declared == visibility => {}
            Some(declared) => self.by_hand(format!(
                "`{declared}` at {}:{} does not reach the code that now reads it; it needs `{visibility}`",
                display_path(&document.path),
                line + 1
            )),
        }
    }

    /// Plans `item`, a `use` binding `name` to `meaning`, for the file `path`.
    /// `false` where the file's plan already binds the name to something else,
    /// which leaves the plan as it was.
    fn import(&mut self, path: &Path, name: &str, item: String, meaning: Meaning) -> bool {
        let planned = self.imports.entry(path.to_path_buf()).or_default();
        if let Some((_, other)) = planned.get(name) {
            return *other == meaning;
        }
        planned.insert(name.to_string(), (item, meaning));
        true
    }

    /// [`Self::import`] of `name`, which means `meaning`, into `document` from
    /// its sibling `from`.
    fn import_from_sibling(
        &mut self,
        document: &Document,
        from: &Document,
        name: &str,
        meaning: Meaning,
    ) -> bool {
        let module = from.module_name().expect("siblings name modules");
        let item = format!("use super::{module}::{name};");
        self.import(&document.path, name, item, meaning)
    }

    fn by_hand(&mut self, what: String) {
        let line = format!("by hand: {what}");
        if !self.report.contains(&line) {
            self.report.push(line);
        }
    }
}

/// The items a move takes and the files it takes them between.
struct Move {
    destination: Document,
    destination_uses: Vec<UseItem>,
    sources: BTreeMap<PathBuf, Document>,
    moved: Vec<Moved>,
}

impl Move {
    /// What the destination's top level means by `name` once the items have
    /// landed; `None` where it binds no such name.
    fn destination_meaning(
        &self,
        client: &mut Client,
        name: &str,
    ) -> Result<Option<Meaning>, String> {
        let destination = &self.destination;
        // A top-level item that moves is the destination's own from here on.
        if let Some(item) = self
            .moved
            .iter()
            .find(|item| item.header.is_none() && item.name == name)
        {
            let uri = self.sources[&item.source].uri.clone();
            let Position { line, column } = item.name_position;
            return Ok(Some(Meaning::from([(uri, line, column)])));
        }
        if let Some(symbol) = destination.symbols.iter().find(|s| symbol_name(s) == name) {
            let Position { line, column } = name_position(symbol);
            return Ok(Some(Meaning::from([(
                destination.uri.clone(),
                line,
                column,
            )])));
        }
        for item in &self.destination_uses {
            if let Some(binding) = item.bindings.iter().find(|b| b.name == name) {
                return destination.imported(client, binding).map(Some);
            }
        }
        Ok(None)
    }

    /// Reports `name`, which the moved code reads as one thing and the
    /// destination binds as another.
    fn report_rebinding(&self, fixes: &mut Fixes, name: &str, source: &Document) {
        fixes.by_hand(format!(
            "`{name}` means one thing in {} and another in {}, where the moved code now reads it",
            display_path(&source.path),
            display_path(&self.destination.path)
        ));
    }

    fn is_moved(&self, path: &Path, line: usize) -> bool {
        self.moved
            .iter()
            .any(|item| item.source == path && item.span.contains(&line))
    }

    /// An item the move takes out of the reach of who refers to it.
    fn keep_moved_items_in_reach(
        &self,
        client: &mut Client,
        fixes: &mut Fixes,
    ) -> Result<(), String> {
        let destination = &self.destination;
        for item in &self.moved {
            let source = &self.sources[&item.source];
            let references = client.request(
                "textDocument/references",
                json!({
                    "textDocument": { "uri": source.uri },
                    "position": item.name_position.to_lsp(),
                    "context": { "includeDeclaration": false },
                }),
            )?;
            let references: Vec<(PathBuf, Position)> = locations(&references)
                .into_iter()
                .map(|reference| (uri_path(&reference.uri), reference.position))
                .collect();
            // A path the destination names a free item by goes through the
            // file the item leaves.
            if item.header.is_none() {
                for (_, at) in references
                    .iter()
                    .filter(|(path, _)| *path == destination.path)
                {
                    let before = destination.lines[at.line].get(..at.column);
                    if before.is_some_and(|before| before.ends_with("::")) {
                        fixes.by_hand(format!(
                            "{} names `{}` by a path",
                            display_position(&destination.path, *at),
                            item.name
                        ));
                    }
                }
            }
            let users: BTreeSet<PathBuf> = references
                .into_iter()
                .filter(|(path, at)| *path != destination.path && !self.is_moved(path, at.line))
                .map(|(path, _)| path)
                .collect();
            if users.is_empty() {
                continue;
            }
            fixes.widen(
                source,
                item.name_position.line,
                visibility_reaching(&destination.path, &users),
            );
            // A member is reached through its type, wherever its block is; a
            // free item is reached by a path, which names the file it left.
            if item.header.is_some() {
                continue;
            }
            let mut elsewhere = Vec::new();
            for user in &users {
                if *user == item.source && are_siblings(source, destination) {
                    let Position { line, column } = item.name_position;
                    let itself = Meaning::from([(source.uri.clone(), line, column)]);
                    let planned =
                        fixes.import_from_sibling(source, destination, &item.name, itself);
                    assert!(planned, "a file declares one item of a name");
                } else {
                    elsewhere.push(display_path(user));
                }
            }
            if !elsewhere.is_empty() {
                fixes.by_hand(format!(
                    "`{}` is named in {}",
                    item.name,
                    elsewhere.join(", ")
                ));
            }
        }
        Ok(())
    }

    /// What the moved code reads of the file it leaves: a private item there
    /// is now out of reach, and a free one needs an import.
    fn keep_source_items_in_reach(
        &self,
        client: &mut Client,
        fixes: &mut Fixes,
    ) -> Result<(), String> {
        let destination = &self.destination;
        for item in &self.moved {
            let source = &self.sources[&item.source];
            let mut read: BTreeSet<(usize, usize)> = BTreeSet::new();
            for (position, name) in source.mentions(item) {
                if is_keyword(name) {
                    continue;
                }
                for (uri, line, column) in source.definitions(client, *position)? {
                    if uri == source.uri && !self.is_moved(&source.path, line) {
                        read.insert((line, column));
                    }
                }
            }
            let visibility = visibility_reaching(&source.path, [&destination.path]);
            for (line, column) in read {
                let position = Position { line, column };
                let Some((symbol, parent)) = find_symbol(&source.symbols, position, None) else {
                    continue;
                };
                let kind = symbol["kind"].as_u64();
                // A variant is as visible as its enum. A type alias is no
                // exception to the rest, though the server reports one under
                // the kind of a type parameter.
                if kind == Some(ENUM_MEMBER_KIND) {
                    continue;
                }
                let name = symbol_name(symbol);
                // A macro takes no visibility, and is reached by textual scope
                // or `#[macro_export]`.
                if source.lines[line].trim_start().starts_with("macro_rules!") {
                    fixes.by_hand(format!(
                        "the macro `{name}` of {} is invoked by the moved code",
                        display_path(&source.path)
                    ));
                    continue;
                }
                match parent {
                    // A variant's field is as visible as the enum, which the
                    // moved code may reach through `Self` without naming it.
                    Some(variant) if variant["kind"].as_u64() == Some(ENUM_MEMBER_KIND) => {
                        let (_, holder) =
                            find_symbol(&source.symbols, name_position(variant), None)
                                .expect("the variant was found under its enum");
                        let holder = holder.expect("a variant is a member of an enum");
                        fixes.widen(source, name_position(holder).line, visibility);
                        continue;
                    }
                    // A trait's member is as visible as the trait.
                    Some(parent)
                        if is_trait_impl(parent)
                            || (!is_impl(parent) && kind != Some(FIELD_KIND)) =>
                    {
                        if parent["kind"].as_u64() == Some(MODULE_KIND) {
                            fixes.by_hand(format!(
                                "`{name}` in the module `{}` of {} is read by the moved code",
                                symbol_name(parent),
                                display_path(&source.path)
                            ));
                        }
                        continue;
                    }
                    // A field is read off a value, so the moved code may
                    // never name the type holding it; the type is needed all
                    // the same.
                    Some(holder) if !is_impl(holder) => {
                        fixes.widen(source, name_position(holder).line, visibility);
                    }
                    Some(_) => {}
                    None if are_siblings(source, destination) => {
                        let here = Meaning::from([(source.uri.clone(), line, column)]);
                        let same = match self.destination_meaning(client, name)? {
                            None => fixes.import_from_sibling(destination, source, name, here),
                            Some(there) => there == here,
                        };
                        if !same {
                            self.report_rebinding(fixes, name, source);
                        }
                    }
                    None => fixes.by_hand(format!(
                        "import `{name}` of {} into {}",
                        display_path(&source.path),
                        display_path(&destination.path)
                    )),
                }
                fixes.widen(source, line, visibility);
            }
        }
        Ok(())
    }

    /// The imports the moved code reads by name.
    fn repeat_imports(&self, client: &mut Client, fixes: &mut Fixes) -> Result<(), String> {
        let destination = &self.destination;
        for source in self.sources.values() {
            let mentioned: BTreeSet<&str> = self
                .moved
                .iter()
                .filter(|item| item.source == source.path)
                .flat_map(|item| source.mentions(item))
                .map(|(_, name)| name.as_str())
                .collect();
            for item in use_items(&source.text).iter().filter(|item| item.plain) {
                for binding in &item.bindings {
                    if !mentioned.contains(binding.name.as_str()) {
                        continue;
                    }
                    let here = source.imported(client, binding)?;
                    if let Some(there) = self.destination_meaning(client, &binding.name)? {
                        if there != here {
                            self.report_rebinding(fixes, &binding.name, source);
                        }
                        continue;
                    }
                    // `self` and a child module name the source file itself,
                    // and `super` the parent only a sibling shares.
                    let first = binding.first_segment();
                    let relative = first == "self"
                        || source.symbols.iter().any(|s| symbol_name(s) == first)
                        || (first == "super" && !are_siblings(source, destination));
                    if relative {
                        fixes.by_hand(format!(
                            "`{}` of {} is relative to it",
                            binding.item(),
                            display_path(&source.path)
                        ));
                        continue;
                    }
                    // Another source may have had the name planned already.
                    if !fixes.import(&destination.path, &binding.name, binding.item(), here) {
                        self.report_rebinding(fixes, &binding.name, source);
                    }
                }
                for glob in &item.globs {
                    if !self.destination_uses.iter().any(|d| d.globs.contains(glob)) {
                        fixes.by_hand(format!(
                            "the moved code may read `use {glob}::*;` of {}",
                            display_path(&source.path)
                        ));
                    }
                }
            }
        }
        Ok(())
    }

    /// The new text of each file, and the account of what changed.
    fn write(&self, fixes: Fixes) -> Plan {
        let Fixes {
            mut report,
            widenings,
            imports,
        } = fixes;
        let imports_of = |path: &Path| -> BTreeSet<String> {
            imports
                .get(path)
                .map(|by_name| by_name.values().map(|(item, _)| item.clone()).collect())
                .unwrap_or_default()
        };
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
        for path in imports.keys() {
            for item in imports_of(path) {
                report.push(format!("import in {}: {item}", display_path(path)));
            }
        }
        for item in &self.moved {
            report.push(format!(
                "move {}:{} `{}` to {}",
                display_path(&item.source),
                item.span.start + 1,
                item.name,
                display_path(&self.destination.path)
            ));
        }
        report.push(
            "next: `cargo check` names a trait import the moved code calls a method of, \
             and `cargo clippy --fix` removes the imports left unused"
                .to_string(),
        );

        // The lines are widened in place first, and cut or spliced after.
        let mut files = BTreeMap::new();
        let mut taken: Vec<Option<Taken>> = self.moved.iter().map(|_| None).collect();
        for source in self.sources.values() {
            let mut lines = source.lines.clone();
            for widening in widenings.get(&source.path).into_iter().flatten() {
                let line = &mut lines[widening.line];
                let indent = line.len() - line.trim_start().len();
                line.insert_str(indent, &format!("{} ", widening.visibility));
            }
            let mut cut: Vec<Range<usize>> = Vec::new();
            let own = self
                .moved
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
            files.insert(
                source.path.clone(),
                render(without(lines, &cut), &imports_of(&source.path), &[], source),
            );
        }
        let taken: Vec<Taken> = taken
            .into_iter()
            .map(|item| item.expect("every moved item has a source"))
            .collect();
        files.insert(
            self.destination.path.clone(),
            render(
                self.destination.lines.clone(),
                &imports_of(&self.destination.path),
                &taken,
                &self.destination,
            ),
        );
        Plan { files, report }
    }
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
        kind: symbol["kind"].as_u64(),
        name_position: position,
        header: block.map(|block| impl_header(source, block)),
        header_span: block.map(|block| line_span(block).start..impl_header_line(source, block) + 1),
        emptied_block: None,
    })
}

/// Marks the members of each `impl` block the move takes every member of, so
/// the block goes with them.
fn mark_emptied_blocks(sources: &BTreeMap<PathBuf, Document>, moved: &mut [Moved]) {
    for source in sources.values() {
        for block in source.symbols.iter().filter(|s| is_impl(s)) {
            let block_span = line_span(block);
            let leaves = |line: usize| {
                moved
                    .iter()
                    .any(|item| item.source == source.path && item.span.contains(&line))
            };
            // Not by its symbols alone: the server lists neither a comment nor
            // a macro invocation, and the block keeps what does not move.
            let body = impl_header_line(source, block) + 1..block_span.end - 1;
            let all_move = !children(block).is_empty()
                && body
                    .into_iter()
                    .all(|line| leaves(line) || source.lines[line].trim().is_empty());
            if !all_move {
                continue;
            }
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
        // Else below the file's own doc comment and attributes, which no item
        // may come ahead of.
        let at = after_uses.unwrap_or_else(|| {
            let is_inner = |line: &String| line.starts_with("//!") || line.starts_with("#![");
            let preamble = lines
                .iter()
                .take_while(|line| line.is_empty() || is_inner(line))
                .count();
            lines[..preamble]
                .iter()
                .rposition(is_inner)
                .map_or(0, |last| last + 1)
        });
        let mut block: Vec<String> = imports.iter().cloned().collect();
        if after_uses.is_none() {
            if at > 0 {
                block.insert(0, String::new());
            }
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
    fn a_type_alias_the_moved_code_reads_follows_it() {
        let dir = probe(&[
            ("src/main.rs", "mod a;\nmod b;\nfn main() { a::pair(); }\n"),
            (
                "src/a.rs",
                "type Pair = (i32, i32);\n\npub fn pair() -> i32 {\n    make().0\n}\n\n\
                 fn make() -> Pair {\n    (1, 2)\n}\n",
            ),
            ("src/b.rs", ""),
        ]);
        move_items(
            dir.path(),
            &locators(&["probe::a::make"]),
            &dir.path().join("src/b.rs"),
            Mode::Apply,
        )
        .unwrap();
        assert!(read(&dir, "src/a.rs").starts_with("use super::b::make;\n\npub(super) type Pair"));
        assert!(read(&dir, "src/b.rs").contains("use super::a::Pair;"));
    }

    /// Moves `names` out of `a` into `b`, two sibling modules of a crate whose
    /// `main` reads `uses`, and returns the workspace and the report.
    fn a_to_b(a: &str, b: &str, uses: &str, names: &[&str]) -> (TempDir, String) {
        let main = format!("mod a;\nmod b;\nfn main() {{ {uses}; }}\n");
        let dir = probe(&[("src/main.rs", &main), ("src/a.rs", a), ("src/b.rs", b)]);
        let report = move_items(
            dir.path(),
            &locators(names),
            &dir.path().join("src/b.rs"),
            Mode::Apply,
        )
        .unwrap();
        (dir, report.join("\n"))
    }

    #[test]
    fn a_name_the_destination_binds_otherwise_is_reported() {
        let (dir, report) = a_to_b(
            "use std::collections::BTreeMap as Map;\nuse std::collections::BTreeSet;\n\n\
             pub fn compute(n: i32) -> i32 {\n    let m: Map<i32, BTreeSet<i32>> = Map::new();\n    \
             scale(n) + m.len() as i32\n}\n\nfn scale(n: i32) -> i32 {\n    n * 2\n}\n",
            "use std::collections::HashMap as Map;\nuse std::collections::{BTreeSet};\n\n\
             pub fn other() -> usize {\n    let seen: BTreeSet<i32> = BTreeSet::new();\n    \
             Map::<i32, i32>::new().len() + seen.len() + scale(1) as usize\n}\n\n\
             fn scale(n: i32) -> i32 {\n    n * 100\n}\n",
            "a::compute(1); b::other()",
            &["probe::a::compute"],
        );
        assert!(
            report.contains("by hand: `scale` means one thing"),
            "{report}"
        );
        assert!(
            report.contains("by hand: `Map` means one thing"),
            "{report}"
        );
        assert!(!report.contains("`BTreeSet`"), "{report}");
        let b = read(&dir, "src/b.rs");
        assert!(!b.contains("use super::a::scale;"), "{b}");
        assert!(!b.contains("BTreeMap"), "{b}");
    }

    #[test]
    fn what_takes_no_visibility_is_not_widened() {
        let (dir, report) = a_to_b(
            "enum Shape {\n    Circle { radius: i32 },\n    Dot,\n}\n\n\
             macro_rules! twice {\n    ($e:expr) => {\n        $e * 2\n    };\n}\n\n\
             pub fn run() -> i32 {\n    area(&Shape::Circle { radius: 1 }) + area(&Shape::Dot)\n}\n\n\
             fn area(shape: &Shape) -> i32 {\n    match shape {\n        \
             Shape::Circle { radius } => twice!(*radius),\n        Shape::Dot => 0,\n    }\n}\n",
            "",
            "a::run()",
            &["probe::a::area"],
        );
        let a = read(&dir, "src/a.rs");
        assert!(
            a.contains("pub(super) enum Shape {\n    Circle { radius: i32 },"),
            "{a}"
        );
        assert!(a.contains("\nmacro_rules! twice {"), "{a}");
        assert!(report.contains("by hand: the macro `twice`"), "{report}");
        assert!(!read(&dir, "src/b.rs").contains("twice;"));
    }

    #[test]
    fn a_stale_path_and_a_short_visibility_are_reported() {
        let (dir, report) = a_to_b(
            "pub fn make() -> i32 {\n    base()\n}\n\npub(self) fn base() -> i32 {\n    1\n}\n",
            "#![allow(dead_code)]\n\npub fn twice() -> i32 {\n    super::a::make() * 2\n}\n",
            "b::twice()",
            &["probe::a::make"],
        );
        assert!(
            report.contains("src/b.rs:4:15 names `make` by a path"),
            "{report}"
        );
        assert!(report.contains("src/a.rs:5 does not reach"), "{report}");
        let b = read(&dir, "src/b.rs");
        assert!(
            b.starts_with("#![allow(dead_code)]\n\nuse super::a::base;\n\npub fn twice"),
            "{b}"
        );
    }

    #[test]
    fn a_second_declaration_of_a_name_is_refused() {
        let main = "mod a;\nmod b;\nfn main() { a::reset(); b::reset(); }\n";
        let dir = probe(&[
            ("src/main.rs", main),
            ("src/a.rs", "pub fn reset() {}\n"),
            ("src/b.rs", "pub fn reset() {}\n"),
        ]);
        let error = move_items(
            dir.path(),
            &locators(&["probe::a::reset"]),
            &dir.path().join("src/b.rs"),
            Mode::Apply,
        )
        .unwrap_err();
        assert!(error.contains("would declare `reset` twice"), "{error}");
    }

    #[test]
    fn two_sources_binding_one_name_differently_are_reported() {
        let dir = probe(&[
            (
                "src/main.rs",
                "mod a;\nmod b;\nmod c;\nfn main() { a::one(); c::two(); }\n",
            ),
            (
                "src/a.rs",
                "use std::collections::BTreeMap as Map;\n\npub fn one() -> usize {\n    \
                 Map::<i32, i32>::new().len()\n}\n",
            ),
            ("src/b.rs", ""),
            (
                "src/c.rs",
                "use std::collections::HashMap as Map;\n\npub fn two() -> usize {\n    \
                 Map::<i32, i32>::new().len()\n}\n",
            ),
        ]);
        let report = move_items(
            dir.path(),
            &locators(&["probe::a::one", "probe::c::two"]),
            &dir.path().join("src/b.rs"),
            Mode::Apply,
        )
        .unwrap()
        .join("\n");
        assert!(
            report.contains("by hand: `Map` means one thing in"),
            "{report}"
        );
        assert!(!read(&dir, "src/b.rs").contains("HashMap"));
    }

    #[test]
    fn a_block_keeps_what_does_not_move() {
        let (dir, _) = a_to_b(
            "pub struct S;\n\nimpl S {\n    pub fn one(&self) -> i32 {\n        1\n    }\n\n    \
             // A note the block keeps.\n}\n",
            "",
            "a::S.one()",
            &["probe::a::S::one"],
        );
        let a = read(&dir, "src/a.rs");
        assert!(
            a.contains("impl S {\n    // A note the block keeps.\n}"),
            "{a}"
        );
    }

    #[test]
    fn an_import_through_a_child_module_is_not_repeated() {
        let dir = probe(&[
            ("src/main.rs", "mod a;\nmod b;\nfn main() { a::make(); }\n"),
            (
                "src/a.rs",
                "mod inner;\n\nuse inner::Thing;\n\npub fn keep() -> Thing {\n    Thing\n}\n\n\
                 pub fn make() -> Thing {\n    Thing\n}\n",
            ),
            ("src/a/inner.rs", "pub struct Thing;\n"),
            ("src/b.rs", ""),
        ]);
        let report = move_items(
            dir.path(),
            &locators(&["probe::a::make"]),
            &dir.path().join("src/b.rs"),
            Mode::Apply,
        )
        .unwrap()
        .join("\n");
        assert!(report.contains("by hand: `use inner::Thing;`"), "{report}");
        assert!(!read(&dir, "src/b.rs").contains("use inner::Thing;"));
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
