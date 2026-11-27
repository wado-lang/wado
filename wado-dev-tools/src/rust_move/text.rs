//! What `rust-move` reads off Rust source as text: the identifiers a span
//! mentions and the names a file's `use` items bind. The code is rustfmt's
//! output, so a top-level `use` starts its line.

use std::ops::Range;

const KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];

pub fn is_keyword(word: &str) -> bool {
    KEYWORDS.contains(&word)
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

/// The length of the comment or the string, character or lifetime `rest`
/// opens with: what holds no code.
fn opaque_len(rest: &str) -> Option<usize> {
    if rest.starts_with("//") {
        Some(rest.find('\n').unwrap_or(rest.len()))
    } else if rest.starts_with("/*") {
        Some(block_comment_len(rest))
    } else if rest.starts_with('"') {
        Some(string_len(rest))
    } else if rest.starts_with('\'') {
        Some(quote_len(rest))
    } else {
        raw_string_len(rest)
    }
}

/// Every identifier in `text` with its byte offset, keywords included. A
/// comment, a string, a character and a lifetime hold none.
pub fn identifiers(text: &str) -> Vec<(usize, &str)> {
    let bytes = text.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &text[at..];
        let c = rest.chars().next().expect("`at` is on a character");
        if let Some(len) = opaque_len(rest) {
            at += len;
        } else if is_ident_start(c) {
            let len = rest.find(|c| !is_ident_continue(c)).unwrap_or(rest.len());
            // `b"…"` and `c"…"` are strings, and the prefix is not a name.
            if !(matches!(&rest[..len], "b" | "c") && rest[len..].starts_with('"')) {
                found.push((at, &rest[..len]));
            }
            at += len;
        } else if c.is_ascii_digit() {
            // A literal's suffix (`1u8`, `0xff`) is not a name.
            at += rest.find(|c| !is_ident_continue(c)).unwrap_or(rest.len());
        } else {
            at += c.len_utf8();
        }
    }
    found
}

/// `text` with every comment and literal blanked, byte for byte and line for
/// line: what is left is code, at the offsets it has in `text`.
pub fn code_only(text: &str) -> String {
    let mut code = String::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let c = rest.chars().next().expect("`at` is on a character");
        match opaque_len(rest) {
            Some(len) => {
                code.extend(
                    rest[..len]
                        .bytes()
                        .map(|byte| if byte == b'\n' { '\n' } else { ' ' }),
                );
                at += len;
            }
            // An identifier whole, so its last letter never opens a raw string.
            None if is_ident_start(c) => {
                let len = rest.find(|c| !is_ident_continue(c)).unwrap_or(rest.len());
                code.push_str(&rest[..len]);
                at += len;
            }
            None => {
                code.push(c);
                at += c.len_utf8();
            }
        }
    }
    code
}

fn block_comment_len(rest: &str) -> usize {
    let mut depth = 0;
    let mut at = 0;
    while at < rest.len() {
        if rest[at..].starts_with("/*") {
            depth += 1;
            at += 2;
        } else if rest[at..].starts_with("*/") {
            depth -= 1;
            at += 2;
            if depth == 0 {
                break;
            }
        } else {
            at += rest[at..].chars().next().map_or(1, char::len_utf8);
        }
    }
    at
}

/// The length of the string literal `rest` opens with.
fn string_len(rest: &str) -> usize {
    let mut chars = rest.char_indices().skip(1);
    while let Some((at, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '"' => return at + 1,
            _ => {}
        }
    }
    rest.len()
}

/// The length of the character literal or lifetime `rest` opens with.
fn quote_len(rest: &str) -> usize {
    let mut chars = rest.chars().skip(1);
    match chars.next() {
        // The escaped character may be the quote itself.
        Some('\\') => rest
            .get(3..)
            .and_then(|after| after.find('\''))
            .map_or(rest.len(), |end| end + 4),
        Some(c) if chars.next() == Some('\'') => 2 + c.len_utf8(),
        // A lifetime: the name after the quote is no item's.
        Some(_) => {
            1 + rest[1..]
                .find(|c| !is_ident_continue(c))
                .unwrap_or(rest.len() - 1)
        }
        None => 1,
    }
}

/// The length of the raw string `rest` opens with (`r"…"`, `br#"…"#`).
fn raw_string_len(rest: &str) -> Option<usize> {
    let prefix = ["br", "cr", "r"].iter().find(|p| rest.starts_with(**p))?;
    let after = &rest[prefix.len()..];
    let hashes = after.len() - after.trim_start_matches('#').len();
    if !after[hashes..].starts_with('"') {
        return None;
    }
    let open = prefix.len() + hashes + 1;
    let close = format!("\"{}", "#".repeat(hashes));
    Some(
        rest[open..]
            .find(&close)
            .map_or(rest.len(), |end| open + end + close.len()),
    )
}

/// One name a `use` item binds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    /// The path as written, from its first segment (`crate::geo::Point`).
    pub path: String,
    pub name: String,
    /// The byte offset of the path's last segment, which names the item
    /// whatever alias binds it.
    pub at: usize,
}

impl Binding {
    pub fn first_segment(&self) -> &str {
        self.path
            .split("::")
            .next()
            .expect("split yields a first part")
    }

    /// The segment naming the item, which an alias does not change.
    pub fn last_segment(&self) -> &str {
        self.path
            .rsplit("::")
            .next()
            .expect("split yields a first part")
    }

    pub fn item(&self) -> String {
        if self.last_segment() == self.name {
            format!("use {};", self.path)
        } else {
            format!("use {} as {};", self.path, self.name)
        }
    }
}

/// A `use` item at the top level of a file.
#[derive(Debug, PartialEq, Eq)]
pub struct UseItem {
    pub lines: Range<usize>,
    pub bindings: Vec<Binding>,
    /// The paths it imports every name of (`crate::geo::*`).
    pub globs: Vec<String>,
    /// Written as `use …`, with neither a visibility nor an attribute: what
    /// another file may repeat as it stands.
    pub plain: bool,
}

/// The top-level `use` items of `text`, in order.
pub fn use_items(text: &str) -> Vec<UseItem> {
    // A line of a comment or a string may start with `use` too, and a comment
    // may sit between two leaves.
    let code = code_only(text);
    let lines: Vec<&str> = code.lines().collect();
    let mut items = Vec::new();
    let mut at = 0;
    let mut line_start = 0;
    while at < lines.len() {
        let line = lines[at];
        let Some(tree_at) = use_keyword_end(line) else {
            line_start += line.len() + 1;
            at += 1;
            continue;
        };
        let start = at;
        let tree_start = line_start + tree_at;
        let tree_len = code[tree_start..]
            .find(';')
            .expect("a `use` item ends in `;`");
        let tree = &code[tree_start..tree_start + tree_len];
        while line_start <= tree_start + tree_len {
            line_start += lines[at].len() + 1;
            at += 1;
        }
        let mut item = UseItem {
            lines: start..at,
            bindings: Vec::new(),
            globs: Vec::new(),
            plain: line.starts_with("use ")
                && !start
                    .checked_sub(1)
                    .is_some_and(|above| lines[above].starts_with("#[")),
        };
        flatten_use_tree(&code, "", tree, &mut item);
        items.push(item);
    }
    items
}

/// Where the tree of a top-level `use` on `line` starts.
fn use_keyword_end(line: &str) -> Option<usize> {
    let after_visibility = match line.strip_prefix("pub") {
        Some(rest) => {
            let rest = match rest.strip_prefix('(') {
                Some(scope) => &scope[scope.find(')')? + 1..],
                None => rest,
            };
            rest.strip_prefix(' ')?
        }
        None => line,
    };
    after_visibility
        .starts_with("use ")
        .then(|| line.len() - after_visibility.len() + 4)
}

/// `tree`, a slice of `code`, as the bindings and globs it writes under
/// `prefix`.
fn flatten_use_tree(code: &str, prefix: &str, tree: &str, out: &mut UseItem) {
    let unspaced = |path: &str| path.split_whitespace().collect::<String>();
    let tree = tree.trim();
    let join = |segment: &str| {
        if prefix.is_empty() {
            segment.to_string()
        } else {
            format!("{prefix}::{segment}")
        }
    };
    if let Some(open) = tree.find('{') {
        let head = tree[..open].trim().trim_end_matches("::").trim();
        let inner = &tree[open + 1..tree.rfind('}').expect("a use group closes")];
        let prefix = if head.is_empty() {
            prefix.to_string()
        } else {
            join(&unspaced(head))
        };
        for part in split_top_level(inner) {
            flatten_use_tree(code, &prefix, part, out);
        }
        return;
    }
    if tree.is_empty() {
        return;
    }
    let (written, alias) = match tree.split_once(" as ") {
        Some((path, alias)) => (path.trim(), Some(alias.trim())),
        None => (tree, None),
    };
    let last_segment = written.rfind("::").map_or(0, |at| at + 2);
    // `written` is a slice of `code`, so the two addresses give its offset.
    let at = written.as_ptr() as usize - code.as_ptr() as usize + last_segment;
    let path = unspaced(written);
    if let Some(parent) = path.strip_suffix("::*") {
        out.globs.push(join(parent));
        return;
    }
    if path == "*" {
        out.globs.push(prefix.to_string());
        return;
    }
    // `geo::{self}` binds `geo`.
    let path = if path == "self" {
        prefix.to_string()
    } else {
        join(&path)
    };
    let name = alias
        .or_else(|| path.rsplit("::").next())
        .expect("a path has a last segment")
        .to_string();
    if name != "_" {
        out.bindings.push(Binding { path, name, at });
    }
}

/// `inner` split at the commas outside any nested group.
fn split_top_level(inner: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0;
    let mut start = 0;
    for (at, c) in inner.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth -= 1,
            ',' if depth == 0 => {
                parts.push(&inner[start..at]);
                start = at + 1;
            }
            _ => {}
        }
    }
    parts.push(&inner[start..]);
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(text: &str) -> Vec<&str> {
        identifiers(text)
            .into_iter()
            .map(|(_, name)| name)
            .collect()
    }

    #[test]
    fn identifiers_skip_what_names_nothing() {
        let text = "fn f<'a>(x: &'a str) -> char { // not_this\n\
                    /* nor /* this */ one */ let s = \"quoted \\\" name\"; let c = 'c'; \
                    let n = '\\n'; let r = r#\"raw \" name\"#; let b = b\"bytes\"; g(x, 1u8) }";
        assert_eq!(
            names(text),
            [
                "fn", "f", "x", "str", "char", "let", "s", "let", "c", "let", "n", "let", "r",
                "let", "b", "g", "x"
            ]
        );
    }

    #[test]
    fn identifiers_carry_their_offsets() {
        let text = "let あ = b;";
        assert_eq!(identifiers(text), [(0, "let"), (4, "あ"), (10, "b")]);
    }

    #[test]
    fn use_items_flatten_their_trees() {
        let text = "//! Doc.\n\nuse std::fmt;\nuse crate::geo::{\n    self, Point as P,\n    \
                    shape::{Circle, Square},\n};\npub(crate) use super::all::*;\n\
                    #[cfg(test)]\nuse std::fs;\n\nfn f() {}\n";
        let items = use_items(text);
        let bound = |item: &UseItem| -> Vec<(String, String)> {
            item.bindings
                .iter()
                .map(|b| (b.path.clone(), b.name.clone()))
                .collect()
        };
        let pair = |path: &str, name: &str| (path.to_string(), name.to_string());
        assert_eq!(items.len(), 4);
        assert_eq!((items[0].lines.clone(), items[0].plain), (2..3, true));
        assert_eq!(bound(&items[0]), [pair("std::fmt", "fmt")]);
        assert_eq!(items[1].lines, 3..7);
        assert_eq!(
            bound(&items[1]),
            [
                pair("crate::geo", "geo"),
                pair("crate::geo::Point", "P"),
                pair("crate::geo::shape::Circle", "Circle"),
                pair("crate::geo::shape::Square", "Square"),
            ]
        );
        assert_eq!(items[1].bindings[1].item(), "use crate::geo::Point as P;");
        assert_eq!(
            (items[2].globs.as_slice(), items[2].plain),
            (&["super::all".to_string()][..], false)
        );
        assert!(!items[3].plain);
    }

    #[test]
    fn use_items_are_items_and_not_the_word() {
        let text = "pub const USAGE: &str = \"we use this\";\n\
                    pub fn last() -> i32 { // callers use this\n    1\n}\n\
                    /*\nuse in::a_comment\n*/\nconst S: &str = \"\nuse in::a_string;\n\";\n";
        assert_eq!(use_items(text), []);
    }

    #[test]
    fn a_binding_is_where_its_item_is_named() {
        let text = "use std::{\n    fmt, // formatter\n    io::Error as IoError,\n    \
                    fmt::Error as FmtError,\n};\n";
        let items = use_items(text);
        let named: Vec<(&str, &str)> = items[0]
            .bindings
            .iter()
            .map(|b| (b.name.as_str(), &text[b.at..b.at + b.last_segment().len()]))
            .collect();
        assert_eq!(
            named,
            [("fmt", "fmt"), ("IoError", "Error"), ("FmtError", "Error")]
        );
        let at = |name: &str| {
            items[0]
                .bindings
                .iter()
                .find(|b| b.name == name)
                .unwrap()
                .at
        };
        assert!(at("IoError") < at("FmtError"));
        assert_eq!(items[0].lines, 0..5);
    }
}
