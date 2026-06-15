//! Parser for Wado **symbol notation** — the official textual name for
//! "this symbol in this module" used by docs and `wado query`.
//!
//! Grammar (see `docs/wep-2026-06-14-symbol-notation.md`):
//!
//! ```text
//! NOTATION := MODULE '#' SYMBOL
//! MODULE   := '"' <import specifier> '"' | <import specifier without '#'>
//! SYMBOL   := <member>                       // free symbol
//!           | RECEIVER '::' <member>          // static-scoped (assoc const/fn, …)
//!           | RECEIVER '.'  <member>          // instance method
//! RECEIVER := <type> | <type> '^' <trait>    // trait impl disambiguation
//! ```
//!
//! `MODULE` is the import specifier verbatim (`core:json`, `./utils.wado`,
//! `https://x/lib.wado`). Quotes are required only when the specifier itself
//! contains `#`; otherwise they may be omitted. `<type>` may carry generics
//! (`List<String>`); `::` / `.` / `^` inside `<…>` are part of the type and
//! never act as separators.

use std::fmt;

/// How the final member attaches to the module / receiver type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    /// A free symbol — function, type, global, … directly in the module.
    Free,
    /// A statically-scoped member reached with `::` (associated const/fn,
    /// static method, nested item).
    Static,
    /// An instance method reached with `.`.
    Method,
}

/// The `Type` or `Type^Trait` receiver preceding a `::` / `.` member.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receiver {
    /// Type name, possibly with generics (`List<String>`).
    pub type_name: String,
    /// Disambiguating trait when the member comes through a trait impl
    /// (`Type^Trait`).
    pub trait_name: Option<String>,
}

/// A parsed symbol notation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolNotation {
    /// Module reference, verbatim as an import specifier, with surrounding
    /// quotes stripped (`core:json`, `./utils.wado`).
    pub module: String,
    /// Receiver type for [`MemberKind::Static`] / [`MemberKind::Method`];
    /// `None` for [`MemberKind::Free`].
    pub receiver: Option<Receiver>,
    /// Final member name — the symbol within the module or on the type.
    pub member: String,
    /// Which separator joined the member.
    pub kind: MemberKind,
}

/// Failure to parse a symbol notation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

fn err(message: impl Into<String>) -> ParseError {
    ParseError {
        message: message.into(),
    }
}

/// Parse a symbol notation string such as `core:json#parse` or
/// `"https://x/lib.wado"#Foo::bar`.
pub fn parse(input: &str) -> Result<SymbolNotation, ParseError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(err("empty symbol notation"));
    }

    let (module, symbol) = split_module(input)?;
    if module.is_empty() {
        return Err(err("missing module before '#'"));
    }
    if symbol.is_empty() {
        return Err(err("missing symbol after '#'"));
    }

    let (receiver, member, kind) = parse_symbol(symbol)?;
    Ok(SymbolNotation {
        module: module.to_string(),
        receiver,
        member,
        kind,
    })
}

/// Split `MODULE#SYMBOL` into its two halves. A quoted module spans to its
/// closing quote (so a `#` inside it is literal); an unquoted module ends at
/// the first `#`.
fn split_module(input: &str) -> Result<(&str, &str), ParseError> {
    if let Some(rest) = input.strip_prefix('"') {
        let close = rest
            .find('"')
            .ok_or_else(|| err("unterminated quoted module"))?;
        let module = &rest[..close];
        let after = &rest[close + 1..];
        let symbol = after
            .strip_prefix('#')
            .ok_or_else(|| err("expected '#' after quoted module"))?;
        Ok((module, symbol))
    } else {
        let hash = input
            .find('#')
            .ok_or_else(|| err("missing '#' separating module and symbol"))?;
        Ok((&input[..hash], &input[hash + 1..]))
    }
}

/// Parse the `SYMBOL` half into `(receiver, member, kind)`.
fn parse_symbol(symbol: &str) -> Result<(Option<Receiver>, String, MemberKind), ParseError> {
    match rightmost_member_sep(symbol) {
        None => Ok((None, symbol.to_string(), MemberKind::Free)),
        Some((start, end, kind)) => {
            let receiver_str = &symbol[..start];
            let member = &symbol[end..];
            if member.is_empty() {
                return Err(err("missing member after separator"));
            }
            let receiver = parse_receiver(receiver_str)?;
            Ok((Some(receiver), member.to_string(), kind))
        }
    }
}

/// Find the rightmost top-level `::` or `.` member separator, returning its
/// `(start, end, kind)` byte range. Separators inside `<…>` generics are
/// ignored.
fn rightmost_member_sep(symbol: &str) -> Option<(usize, usize, MemberKind)> {
    let bytes = symbol.as_bytes();
    let mut depth: i32 = 0;
    let mut found: Option<(usize, usize, MemberKind)> = None;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'<' => depth += 1,
            b'>' => depth = depth.saturating_sub(1),
            b':' if depth == 0 && bytes.get(i + 1) == Some(&b':') => {
                found = Some((i, i + 2, MemberKind::Static));
                i += 2;
                continue;
            }
            b'.' if depth == 0 => found = Some((i, i + 1, MemberKind::Method)),
            _ => {}
        }
        i += 1;
    }
    found
}

/// Parse a `Type` or `Type^Trait` receiver. The `^` is matched at top level so
/// generic arguments cannot be mistaken for the trait separator.
fn parse_receiver(receiver: &str) -> Result<Receiver, ParseError> {
    if receiver.is_empty() {
        return Err(err("missing receiver type before separator"));
    }
    match top_level_caret(receiver) {
        None => Ok(Receiver {
            type_name: receiver.to_string(),
            trait_name: None,
        }),
        Some(pos) => {
            let type_name = &receiver[..pos];
            let trait_name = &receiver[pos + 1..];
            if type_name.is_empty() {
                return Err(err("missing type before '^'"));
            }
            if trait_name.is_empty() {
                return Err(err("missing trait after '^'"));
            }
            Ok(Receiver {
                type_name: type_name.to_string(),
                trait_name: Some(trait_name.to_string()),
            })
        }
    }
}

/// Byte position of a top-level `^` (outside `<…>`), if any.
fn top_level_caret(s: &str) -> Option<usize> {
    let mut depth: i32 = 0;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'<' => depth += 1,
            b'>' => depth = depth.saturating_sub(1),
            b'^' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(input: &str) -> SymbolNotation {
        parse(input).unwrap_or_else(|e| panic!("parse {input:?} failed: {e}"))
    }

    #[test]
    fn free_function() {
        let n = ok("core:json#parse");
        assert_eq!(n.module, "core:json");
        assert_eq!(n.receiver, None);
        assert_eq!(n.member, "parse");
        assert_eq!(n.kind, MemberKind::Free);
    }

    #[test]
    fn relative_path_unquoted() {
        let n = ok("./utils.wado#helper");
        assert_eq!(n.module, "./utils.wado");
        assert_eq!(n.member, "helper");
        assert_eq!(n.kind, MemberKind::Free);
    }

    #[test]
    fn static_member() {
        let n = ok("core:collections#TreeMap::new");
        assert_eq!(n.module, "core:collections");
        assert_eq!(
            n.receiver,
            Some(Receiver {
                type_name: "TreeMap".to_string(),
                trait_name: None,
            })
        );
        assert_eq!(n.member, "new");
        assert_eq!(n.kind, MemberKind::Static);
    }

    #[test]
    fn associated_const_on_primitive() {
        let n = ok("core:math#f64::PI");
        assert_eq!(n.receiver.unwrap().type_name, "f64");
        assert_eq!(n.member, "PI");
        assert_eq!(n.kind, MemberKind::Static);
    }

    #[test]
    fn instance_method() {
        let n = ok("core:collections#TreeMap.insert");
        assert_eq!(n.receiver.unwrap().type_name, "TreeMap");
        assert_eq!(n.member, "insert");
        assert_eq!(n.kind, MemberKind::Method);
    }

    #[test]
    fn generics_are_not_separators() {
        let n = ok("core:collections#List<String>::len");
        let r = n.receiver.unwrap();
        assert_eq!(r.type_name, "List<String>");
        assert_eq!(r.trait_name, None);
        assert_eq!(n.member, "len");
        assert_eq!(n.kind, MemberKind::Static);
    }

    #[test]
    fn method_on_generic_type() {
        let n = ok("core:collections#Map<String,i32>.get");
        assert_eq!(n.receiver.unwrap().type_name, "Map<String,i32>");
        assert_eq!(n.member, "get");
        assert_eq!(n.kind, MemberKind::Method);
    }

    #[test]
    fn trait_impl_member() {
        let n = ok("core:fmt#Point^Display::fmt");
        let r = n.receiver.unwrap();
        assert_eq!(r.type_name, "Point");
        assert_eq!(r.trait_name.as_deref(), Some("Display"));
        assert_eq!(n.member, "fmt");
        assert_eq!(n.kind, MemberKind::Static);
    }

    #[test]
    fn trait_impl_method_with_generics() {
        let n = ok("core:io#File^Stream<u8>.read");
        let r = n.receiver.unwrap();
        assert_eq!(r.type_name, "File");
        assert_eq!(r.trait_name.as_deref(), Some("Stream<u8>"));
        assert_eq!(n.member, "read");
        assert_eq!(n.kind, MemberKind::Method);
    }

    #[test]
    fn quoted_module_with_fragment() {
        let n = ok("\"https://x/lib.wado#v2\"#foo");
        assert_eq!(n.module, "https://x/lib.wado#v2");
        assert_eq!(n.member, "foo");
        assert_eq!(n.kind, MemberKind::Free);
    }

    #[test]
    fn quoted_module_relative() {
        let n = ok("\"./utils.wado\"#Helper::new");
        assert_eq!(n.module, "./utils.wado");
        assert_eq!(n.receiver.unwrap().type_name, "Helper");
        assert_eq!(n.member, "new");
    }

    #[test]
    fn url_unquoted_splits_at_first_hash() {
        let n = ok("https://x/lib.wado#foo");
        assert_eq!(n.module, "https://x/lib.wado");
        assert_eq!(n.member, "foo");
    }

    #[test]
    fn whitespace_is_trimmed() {
        let n = ok("  core:json#parse  ");
        assert_eq!(n.module, "core:json");
        assert_eq!(n.member, "parse");
    }

    #[test]
    fn nested_static_takes_rightmost() {
        let n = ok("core:a#Outer::Inner::make");
        assert_eq!(n.receiver.unwrap().type_name, "Outer::Inner");
        assert_eq!(n.member, "make");
        assert_eq!(n.kind, MemberKind::Static);
    }

    #[test]
    fn errors() {
        assert!(parse("").is_err());
        assert!(parse("   ").is_err());
        assert!(parse("core:json").is_err(), "missing '#'");
        assert!(parse("#parse").is_err(), "missing module");
        assert!(parse("core:json#").is_err(), "missing symbol");
        assert!(parse("\"core:json#parse").is_err(), "unterminated quote");
        assert!(
            parse("\"core:json\"parse").is_err(),
            "missing '#' after quote"
        );
        assert!(parse("core:a#Foo::").is_err(), "missing member");
        assert!(
            parse("core:a#^Display::f").is_err(),
            "missing type before ^"
        );
        assert!(parse("core:a#Foo^::f").is_err(), "missing trait after ^");
    }
}
