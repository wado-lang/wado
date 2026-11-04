//! An LSP `WorkspaceEdit`, read and applied. Positions are in the UTF-8
//! encoding the client negotiates, so a column is a byte offset in its line.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::lsp::uri_path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub column: usize,
}

impl Position {
    pub fn from_lsp(value: &Value) -> Position {
        let field = |name: &str| {
            let n = value[name]
                .as_u64()
                .unwrap_or_else(|| panic!("position must carry `{name}`: {value}"));
            usize::try_from(n).expect("position fits in usize")
        };
        Position {
            line: field("line"),
            column: field("character"),
        }
    }

    pub fn to_lsp(self) -> Value {
        json!({ "line": self.line, "character": self.column })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct TextEdit {
    pub start: Position,
    pub end: Position,
    pub new_text: String,
}

/// One `documentChanges` entry. They run in order, each against the files
/// as the ones before it left them.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Edit { path: PathBuf, edits: Vec<TextEdit> },
    Rename { from: PathBuf, to: PathBuf },
}

impl Change {
    pub fn paths(&self) -> Vec<&PathBuf> {
        match self {
            Change::Edit { path, .. } => vec![path],
            Change::Rename { from, to } => vec![from, to],
        }
    }
}

pub fn parse_workspace_edit(edit: &Value) -> Vec<Change> {
    let changes = edit["documentChanges"]
        .as_array()
        .unwrap_or_else(|| panic!("a workspace edit must carry documentChanges: {edit}"));
    changes.iter().map(parse_change).collect()
}

fn parse_change(change: &Value) -> Change {
    match change["kind"].as_str() {
        None => Change::Edit {
            path: uri_path(as_str(&change["textDocument"]["uri"])),
            edits: change["edits"]
                .as_array()
                .unwrap_or_else(|| panic!("a text document edit must carry edits: {change}"))
                .iter()
                .map(|edit| TextEdit {
                    start: Position::from_lsp(&edit["range"]["start"]),
                    end: Position::from_lsp(&edit["range"]["end"]),
                    new_text: as_str(&edit["newText"]).to_string(),
                })
                .collect(),
        },
        Some("rename") => Change::Rename {
            from: uri_path(as_str(&change["oldUri"])),
            to: uri_path(as_str(&change["newUri"])),
        },
        Some(kind) => panic!("the client declares only rename operations, got `{kind}`"),
    }
}

fn as_str(value: &Value) -> &str {
    value
        .as_str()
        .unwrap_or_else(|| panic!("expected a string, got {value}"))
}

/// Runs the changes against the file system. A rename never replaces a file.
pub fn apply(changes: &[Change]) -> Result<(), String> {
    for change in changes {
        match change {
            Change::Edit { path, edits } => {
                let text = read(path);
                fs::write(path, apply_text_edits(&text, edits))
                    .map_err(|e| format!("{}: {e}", path.display()))?;
            }
            Change::Rename { from, to } => {
                if to.exists() {
                    return Err(format!(
                        "{} already exists; not renaming {} onto it",
                        to.display(),
                        from.display()
                    ));
                }
                fs::rename(from, to).map_err(|e| format!("{}: {e}", from.display()))?;
            }
        }
    }
    Ok(())
}

pub fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The edits of one `TextDocumentEdit`, all positioned in `text` as given.
pub fn apply_text_edits(text: &str, edits: &[TextEdit]) -> String {
    let mut spans: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|edit| {
            let at = |pos| {
                offset(text, pos).unwrap_or_else(|| panic!("edit position {pos:?} is off the text"))
            };
            (at(edit.start), at(edit.end), edit.new_text.as_str())
        })
        .collect();
    spans.sort_by_key(|&(start, end, _)| (start, end));
    for pair in spans.windows(2) {
        assert!(pair[0].1 <= pair[1].0, "edits must not overlap");
    }
    let mut out = text.to_string();
    for &(start, end, new_text) in spans.iter().rev() {
        out.replace_range(start..end, new_text);
    }
    out
}

/// The byte offset of `pos`, or `None` past a line's end or inside a
/// character. LSP ends a line at `\n`, `\r\n` or `\r`.
pub fn offset(text: &str, pos: Position) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut line_start = 0;
    for _ in 0..pos.line {
        let rest = &bytes[line_start..];
        let terminator = rest.iter().position(|&b| b == b'\n' || b == b'\r')?;
        let width = if rest[terminator..].starts_with(b"\r\n") {
            2
        } else {
            1
        };
        line_start += terminator + width;
    }
    let line_len = bytes[line_start..]
        .iter()
        .position(|&b| b == b'\n' || b == b'\r')
        .unwrap_or(bytes.len() - line_start);
    let at = line_start + pos.column;
    (pos.column <= line_len && text.is_char_boundary(at)).then_some(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(line: usize, column: usize) -> Position {
        Position { line, column }
    }

    #[test]
    fn offset_counts_bytes_and_every_line_terminator() {
        let text = "aあ\r\nb\rc\nd";
        assert_eq!(offset(text, pos(0, 4)), Some(4));
        assert_eq!(offset(text, pos(0, 2)), None, "inside あ");
        assert_eq!(offset(text, pos(0, 5)), None, "past the line");
        assert_eq!(offset(text, pos(1, 0)), Some(6));
        assert_eq!(offset(text, pos(2, 1)), Some(9));
        assert_eq!(offset(text, pos(3, 1)), Some(11));
        assert_eq!(offset(text, pos(4, 0)), None);
    }

    #[test]
    fn text_edits_apply_against_the_original_positions() {
        let edit = |start, end, new_text: &str| TextEdit {
            start: pos(0, start),
            end: pos(0, end),
            new_text: new_text.to_string(),
        };
        let edits = [edit(10, 13, "xyz2"), edit(0, 3, "a"), edit(5, 5, "!")];
        assert_eq!(apply_text_edits("foo, bar, baz", &edits), "a, !bar, xyz2");
    }

    #[test]
    fn workspace_edit_keeps_operation_order() {
        let edit = json!({ "documentChanges": [
            { "textDocument": { "uri": "file:///w/src/a.rs", "version": null },
              "edits": [{ "range": { "start": { "line": 0, "character": 4 },
                                      "end": { "line": 0, "character": 5 } },
                          "newText": "b" }] },
            { "kind": "rename", "oldUri": "file:///w/src/a.rs", "newUri": "file:///w/src/b.rs" },
        ]});
        assert_eq!(
            parse_workspace_edit(&edit),
            [
                Change::Edit {
                    path: PathBuf::from("/w/src/a.rs"),
                    edits: vec![TextEdit {
                        start: pos(0, 4),
                        end: pos(0, 5),
                        new_text: "b".to_string()
                    }],
                },
                Change::Rename {
                    from: PathBuf::from("/w/src/a.rs"),
                    to: PathBuf::from("/w/src/b.rs"),
                },
            ]
        );
    }
}
