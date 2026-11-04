//! `rust-rename`: renames a Rust symbol through rust-analyzer's
//! `textDocument/rename`.
//!
//! ```sh
//! wado-dev-tools rust-rename [--workspace DIR] [--dry-run] LOCATOR NEW_NAME
//! ```
//!
//! LOCATOR is `FILE:LINE:COLUMN`, 1-based with the column in bytes as
//! `rg --column` prints it, or a symbol notation ([`symbol`]). The server is
//! the `rust-analyzer` rustup resolves in DIR (default: the current
//! directory), so `rust-toolchain.toml` pins it. It sees only the cfgs its
//! Cargo workspace enables, so code behind another `cfg` keeps the old name
//! and `cargo check` says where.
//!
//! An edit outside DIR is refused before anything is written: renaming the
//! member of a standard library trait's impl would edit the sysroot.

mod edit;
mod lsp;
mod symbol;

use std::env;
use std::path::{Path, PathBuf};
use std::process;

use lexopt::Arg::{Long, Value as Positional};
use serde_json::json;

use edit::{Change, Position, offset};
use lsp::{Client, file_uri, uri_path};
use symbol::{Location, Notation};

/// `workspace/symbol` answers at most this many symbols; rust-analyzer's own
/// default of 128 cuts off a common name such as `new`.
const SYMBOL_SEARCH_LIMIT: u32 = 100_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Apply,
    DryRun,
}

pub enum Locator {
    Position(Location),
    Symbol(Notation),
}

impl Locator {
    /// Reads a position's file, so a position off the text fails before the
    /// server starts.
    pub fn parse(text: &str) -> Result<Locator, String> {
        let mut parts = text.rsplitn(3, ':');
        let (Some(column), Some(line), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            return Notation::parse(text).map(Locator::Symbol);
        };
        let (Ok(line), Ok(column)) = (line.parse::<usize>(), column.parse::<usize>()) else {
            return Notation::parse(text).map(Locator::Symbol);
        };
        if line == 0 || column == 0 {
            return Err(format!("`{text}`: lines and columns count from 1"));
        }
        let path = Path::new(path)
            .canonicalize()
            .map_err(|e| format!("{path}: {e}"))?;
        let position = Position {
            line: line - 1,
            column: column - 1,
        };
        if offset(&edit::read(&path), position).is_none() {
            return Err(format!("`{text}` is not on a character of the file"));
        }
        Ok(Locator::Position(Location {
            uri: file_uri(&path),
            position,
        }))
    }
}

pub fn run(mut parser: lexopt::Parser) {
    let mut workspace = PathBuf::from(".");
    let mut mode = Mode::Apply;
    let mut positionals = Vec::new();
    while let Some(arg) = parser.next().expect("failed to parse args") {
        match arg {
            Long("workspace") => workspace = parser.value().unwrap().into(),
            Long("dry-run") => mode = Mode::DryRun,
            Positional(value) => positionals.push(value.to_string_lossy().into_owned()),
            _ => panic!("unexpected argument: {arg:?}"),
        }
    }
    let [locator, new_name] = <[String; 2]>::try_from(positionals).unwrap_or_else(|_| {
        panic!("usage: rust-rename [--workspace DIR] [--dry-run] LOCATOR NEW_NAME")
    });
    let result =
        Locator::parse(&locator).and_then(|locator| rename(&workspace, &locator, &new_name, mode));
    match result {
        Ok(changes) => print_changes(&changes, mode),
        Err(message) => {
            eprintln!("error: {message}");
            process::exit(1);
        }
    }
}

/// Renames what `locator` names to `new_name`, and returns the changes made,
/// or with `Mode::DryRun` the changes it would make.
pub fn rename(
    workspace: &Path,
    locator: &Locator,
    new_name: &str,
    mode: Mode,
) -> Result<Vec<Change>, String> {
    let root = workspace
        .canonicalize()
        .map_err(|e| format!("{}: {e}", workspace.display()))?;
    let mut client = start_server(&root);
    let result = request_rename(&mut client, &root, locator, new_name);
    client.shutdown();
    let changes = result?;
    if mode == Mode::Apply {
        edit::apply(&changes)?;
    }
    Ok(changes)
}

fn start_server(root: &Path) -> Client {
    let capabilities = json!({
        "general": { "positionEncodings": ["utf-8"] },
        "textDocument": {
            "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
            "hover": { "contentFormat": ["plaintext"] },
        },
        "workspace": {
            "workspaceEdit": { "documentChanges": true, "resourceOperations": ["rename"] },
        },
        "experimental": { "serverStatusNotification": true },
    });
    let options = json!({
        "workspace": { "symbol": { "search": { "kind": "all_symbols", "limit": SYMBOL_SEARCH_LIMIT } } },
        // Priming indexes every crate up front; a rename asks for only what it reaches.
        "cachePriming": { "enable": false },
        // A session reads each file once; the client watches nothing, so
        // nothing does.
        "files": { "watcher": "client" },
    });
    let client = Client::start("rust-analyzer", root, capabilities, options);
    assert_eq!(
        client.capabilities["positionEncoding"], "utf-8",
        "rust-analyzer must accept the UTF-8 position encoding"
    );
    client
}

fn request_rename(
    client: &mut Client,
    root: &Path,
    locator: &Locator,
    new_name: &str,
) -> Result<Vec<Change>, String> {
    let status = client.wait_until_quiescent();
    let message = status["message"].as_str().unwrap_or_default();
    match status["health"].as_str() {
        Some("error") => {
            return Err(format!(
                "rust-analyzer failed to load the workspace: {message}"
            ));
        }
        Some("warning") => eprintln!("warning: {message}"),
        _ => {}
    }
    let target = match locator {
        Locator::Position(location) => location.clone(),
        Locator::Symbol(notation) => symbol::resolve(client, notation)?,
    };
    let edit = client.request(
        "textDocument/rename",
        json!({
            "textDocument": { "uri": target.uri },
            "position": target.position.to_lsp(),
            "newName": new_name,
        }),
    )?;
    if edit.is_null() {
        return Err(format!(
            "nothing to rename at {}",
            display_location(&target)
        ));
    }
    let changes = edit::parse_workspace_edit(&edit);
    let outside: Vec<String> = changes
        .iter()
        .flat_map(Change::paths)
        .filter(|path| !path.starts_with(root))
        .map(|path| path.display().to_string())
        .collect();
    if !outside.is_empty() {
        return Err(format!(
            "the rename would edit files outside {}:\n  {}",
            root.display(),
            outside.join("\n  ")
        ));
    }
    Ok(changes)
}

fn print_changes(changes: &[Change], mode: Mode) {
    for change in changes {
        match change {
            Change::Edit { path, edits } => {
                if mode == Mode::Apply {
                    println!("edited {} ({} sites)", display_path(path), edits.len());
                    continue;
                }
                let text = edit::read(path);
                for edit in edits {
                    let span =
                        |pos| offset(&text, pos).expect("the server positions edits on the text");
                    println!(
                        "{}:{}:{}: `{}` -> `{}`",
                        display_path(path),
                        edit.start.line + 1,
                        edit.start.column + 1,
                        &text[span(edit.start)..span(edit.end)],
                        edit.new_text
                    );
                }
            }
            Change::Rename { from, to } => {
                let verb = if mode == Mode::Apply {
                    "renamed"
                } else {
                    "rename"
                };
                println!("{verb} {} -> {}", display_path(from), display_path(to));
            }
        }
    }
}

fn display_location(location: &Location) -> String {
    format!(
        "{}:{}:{}",
        display_path(&uri_path(&location.uri)),
        location.position.line + 1,
        location.position.column + 1
    )
}

fn display_path(path: &Path) -> String {
    let cwd = env::current_dir()
        .and_then(|dir| dir.canonicalize())
        .expect("the current directory must resolve");
    path.strip_prefix(&cwd)
        .unwrap_or(path)
        .display()
        .to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    const TOOLCHAIN: &str = include_str!("../../rust-toolchain.toml");

    /// A Cargo package named `probe` holding `files`, pinned to the repository's
    /// toolchain so rustup resolves the same rust-analyzer.
    fn probe(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        let write = |path: &str, text: &str| {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        };
        write("rust-toolchain.toml", TOOLCHAIN);
        write(
            "Cargo.toml",
            "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[workspace]\n",
        );
        for (path, text) in files {
            write(path, text);
        }
        dir
    }

    fn read(dir: &TempDir, path: &str) -> String {
        fs::read_to_string(dir.path().join(path)).unwrap()
    }

    fn symbol(text: &str) -> Locator {
        Locator::parse(text).unwrap()
    }

    const SHAPES: &str = "\
use std::fmt;
pub struct Foo { pub width: i32 }
impl Foo { pub fn area(&self) -> i32 { self.width } }
pub trait Shape { fn area(&self) -> i32; }
impl Shape for Foo { fn area(&self) -> i32 { self.width * 2 } }
impl fmt::Display for Foo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { write!(f, \"{}\", self.area()) }
}
fn main() { let f = Foo { width: 1 }; println!(\"{} {}\", f, Shape::area(&f)); }
";

    #[test]
    fn locator_is_a_position_only_with_two_numbers() {
        let dir = probe(&[("src/main.rs", "fn main() {}\n")]);
        let at = |line_column: &str| {
            format!("{}:{line_column}", dir.path().join("src/main.rs").display())
        };
        assert!(matches!(
            Locator::parse(&at("1:4")).unwrap(),
            Locator::Position(Location {
                position: Position { line: 0, column: 3 },
                ..
            })
        ));
        assert!(matches!(
            Locator::parse("probe::Foo").unwrap(),
            Locator::Symbol(_)
        ));
        assert!(Locator::parse(&at("0:4")).is_err());
        assert!(Locator::parse(&at("1:99")).is_err());
    }

    #[test]
    fn position_columns_count_utf8_bytes() {
        let dir = probe(&[(
            "src/main.rs",
            "fn main() { let s = \"あい\"; let w = 1; println!(\"{s}{w}\"); }\n",
        )]);
        // `w` follows `あい`, six bytes but two characters.
        let column = "fn main() { let s = \"あい\"; let ".len() + 1;
        let locator = Locator::parse(&format!(
            "{}:1:{column}",
            dir.path().join("src/main.rs").display()
        ))
        .unwrap();
        rename(dir.path(), &locator, "width", Mode::Apply).unwrap();
        assert_eq!(
            read(&dir, "src/main.rs"),
            "fn main() { let s = \"あい\"; let width = 1; println!(\"{s}{width}\"); }\n"
        );
    }

    #[test]
    fn symbol_names_a_field() {
        let dir = probe(&[("src/main.rs", SHAPES)]);
        let changes = rename(
            dir.path(),
            &symbol("probe::Foo::width"),
            "breadth",
            Mode::DryRun,
        )
        .unwrap();
        assert_eq!(
            read(&dir, "src/main.rs"),
            SHAPES,
            "a dry run writes nothing"
        );
        edit::apply(&changes).unwrap();
        assert_eq!(
            read(&dir, "src/main.rs"),
            SHAPES.replace("width", "breadth")
        );
    }

    #[test]
    fn symbol_names_an_inherent_method_apart_from_trait_impls() {
        let dir = probe(&[("src/main.rs", SHAPES)]);
        rename(dir.path(), &symbol("probe::Foo::area"), "size", Mode::Apply).unwrap();
        assert_eq!(
            read(&dir, "src/main.rs"),
            SHAPES
                .replace("pub fn area", "pub fn size")
                .replace("self.area()", "self.size()")
        );
    }

    #[test]
    fn symbol_names_a_trait_member_and_renames_its_impls() {
        let dir = probe(&[("src/main.rs", SHAPES)]);
        rename(
            dir.path(),
            &symbol("probe::Shape::area"),
            "surface",
            Mode::Apply,
        )
        .unwrap();
        assert_eq!(
            read(&dir, "src/main.rs"),
            SHAPES
                .replace("fn area(&self) -> i32;", "fn surface(&self) -> i32;")
                .replace(
                    "fn area(&self) -> i32 { self.width * 2",
                    "fn surface(&self) -> i32 { self.width * 2"
                )
                .replace("Shape::area(&f)", "Shape::surface(&f)")
        );
    }

    #[test]
    fn module_rename_moves_its_file_and_directory() {
        let dir = probe(&[
            (
                "src/main.rs",
                "mod geo;\nfn main() { geo::inner::helper(); }\n",
            ),
            ("src/geo.rs", "pub mod inner;\n"),
            ("src/geo/inner.rs", "pub fn helper() {}\n"),
        ]);
        rename(dir.path(), &symbol("probe::geo"), "geom", Mode::Apply).unwrap();
        assert_eq!(
            read(&dir, "src/main.rs"),
            "mod geom;\nfn main() { geom::inner::helper(); }\n"
        );
        assert_eq!(read(&dir, "src/geom.rs"), "pub mod inner;\n");
        assert_eq!(read(&dir, "src/geom/inner.rs"), "pub fn helper() {}\n");
        assert!(!dir.path().join("src/geo.rs").exists());
    }

    #[test]
    fn symbol_resolution_reports_what_it_cannot_settle() {
        let dir = probe(&[(
            "src/main.rs",
            "mod dup { pub fn f() {} }\nfn dup() {}\nfn main() { dup(); dup::f(); }\n",
        )]);
        let ambiguous = rename(dir.path(), &symbol("probe::dup"), "x", Mode::DryRun).unwrap_err();
        assert!(ambiguous.contains("names 2 symbols"), "{ambiguous}");
        let missing = rename(dir.path(), &symbol("probe::nothing"), "x", Mode::DryRun).unwrap_err();
        assert!(missing.contains("no symbol"), "{missing}");
    }

    #[test]
    fn edits_outside_the_workspace_are_refused() {
        let dir = probe(&[("src/main.rs", SHAPES)]);
        let line = SHAPES.lines().position(|l| l.contains("fn fmt")).unwrap() + 1;
        let column = SHAPES.lines().nth(line - 1).unwrap().find("fmt").unwrap() + 1;
        let locator = Locator::parse(&format!(
            "{}:{line}:{column}",
            dir.path().join("src/main.rs").display()
        ))
        .unwrap();
        let error = rename(dir.path(), &locator, "render", Mode::Apply).unwrap_err();
        assert!(error.contains("outside"), "{error}");
        assert_eq!(read(&dir, "src/main.rs"), SHAPES);
    }
}
