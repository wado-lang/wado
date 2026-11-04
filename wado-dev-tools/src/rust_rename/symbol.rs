//! Symbol notation: a Rust path from the crate name down, as rustdoc's
//! intra-doc links write one. `wado_compiler::name::Resolver::resolve` names a
//! method, `wado_compiler::ast::Item::span` a field. A trait-impl member is
//! named through the trait (`crate::Shape::area`), since renaming one renames
//! the trait's and every impl's.
//!
//! rust-analyzer has no lookup by path, so resolution searches
//! `workspace/symbol` for the last segment and keeps the candidates whose
//! hover names the rest of the path. Fields are not workspace symbols, so a
//! field is found as a child of its type in `textDocument/documentSymbol`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::display_location;
use super::edit::Position;
use super::lsp::Client;

/// `SymbolKind.Field` in the LSP specification.
const FIELD_KIND: u64 = 8;

#[derive(Debug, PartialEq, Eq)]
pub struct Notation {
    segments: Vec<String>,
}

impl Notation {
    /// Parses `crate::path::Item`. The crate may be written as its package
    /// name, `-` for `_`.
    pub fn parse(text: &str) -> Result<Notation, String> {
        let mut segments: Vec<String> = text.split("::").map(str::to_string).collect();
        segments[0] = segments[0].replace('-', "_");
        if segments.len() < 2 {
            return Err(format!(
                "`{text}` names no item: write `crate_name::path::Item`"
            ));
        }
        if let Some(bad) = segments.iter().find(|s| !is_identifier(s)) {
            return Err(format!("`{bad}` in `{text}` is not an identifier"));
        }
        Ok(Notation { segments })
    }

    fn path(&self) -> String {
        self.segments.join("::")
    }

    fn parent(&self) -> Option<Notation> {
        (self.segments.len() > 2).then(|| Notation {
            segments: self.segments[..self.segments.len() - 1].to_vec(),
        })
    }

    fn name(&self) -> &str {
        self.segments.last().expect("a notation has segments")
    }
}

fn is_identifier(segment: &str) -> bool {
    let mut chars = segment.chars();
    chars.next().is_some_and(|c| c == '_' || c.is_alphabetic())
        && chars.all(|c| c == '_' || c.is_alphanumeric())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub uri: String,
    pub position: Position,
}

struct Item {
    location: Location,
    /// The item's `DocumentSymbol`, absent for one no document symbol covers,
    /// such as an item a macro defines.
    symbol: Option<Value>,
}

/// The one location `notation` names, or why there is not exactly one.
pub fn resolve(client: &mut Client, notation: &Notation) -> Result<Location, String> {
    let mut resolver = Resolver {
        client,
        document_symbols: BTreeMap::new(),
    };
    let mut found: Vec<Location> = resolver
        .items(notation)?
        .into_iter()
        .map(|item| item.location)
        .collect();
    if let Some(parent) = notation.parent() {
        for item in resolver.items(&parent)? {
            let fields = item.symbol.iter().flat_map(children);
            for field in fields.filter(|f| f["kind"] == FIELD_KIND && f["name"] == notation.name())
            {
                found.push(Location {
                    uri: item.location.uri.clone(),
                    position: Position::from_lsp(&field["selectionRange"]["start"]),
                });
            }
        }
    }
    match found.as_slice() {
        [] => Err(format!(
            "no symbol in the workspace is `{}`",
            notation.path()
        )),
        [one] => Ok(one.clone()),
        many => Err(format!(
            "`{}` names {} symbols; give a position instead:\n{}",
            notation.path(),
            many.len(),
            many.iter()
                .map(|l| format!("  {}", display_location(l)))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    }
}

struct Resolver<'a> {
    client: &'a mut Client,
    document_symbols: BTreeMap<String, Value>,
}

impl Resolver<'_> {
    /// The workspace symbols whose path is `notation`, trait-impl members aside.
    fn items(&mut self, notation: &Notation) -> Result<Vec<Item>, String> {
        let symbols = self
            .client
            .request("workspace/symbol", json!({ "query": notation.name() }))?;
        let mut items = Vec::new();
        for symbol in symbols.as_array().into_iter().flatten() {
            if symbol["name"] != notation.name() {
                continue;
            }
            let location = Location {
                uri: symbol["location"]["uri"]
                    .as_str()
                    .expect("a workspace symbol has a URI")
                    .to_string(),
                position: Position::from_lsp(&symbol["location"]["range"]["start"]),
            };
            let (document_symbol, in_trait_impl) = self.document_symbol(&location);
            if in_trait_impl || self.owner(&location)? + "::" + notation.name() != notation.path() {
                continue;
            }
            items.push(Item {
                location,
                symbol: document_symbol,
            });
        }
        Ok(items)
    }

    /// The document symbol named at `location`, and whether a trait impl
    /// holds it.
    fn document_symbol(&mut self, location: &Location) -> (Option<Value>, bool) {
        let client = &mut *self.client;
        let tree = self
            .document_symbols
            .entry(location.uri.clone())
            .or_insert_with(|| {
                client
                    .request(
                        "textDocument/documentSymbol",
                        json!({ "textDocument": { "uri": location.uri } }),
                    )
                    .unwrap_or_else(|e| panic!("documentSymbol failed: {e}"))
            });
        find_symbol(
            tree.as_array().into_iter().flatten(),
            location.position,
            false,
        )
        .map_or((None, false), |(symbol, in_trait_impl)| {
            (Some(symbol.clone()), in_trait_impl)
        })
    }

    /// The path rust-analyzer's hover puts above an item: its module, or for
    /// an associated item, the type or trait that holds it.
    fn owner(&mut self, location: &Location) -> Result<String, String> {
        let hover = self.client.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": location.uri },
                "position": { "line": location.position.line, "character": location.position.column },
            }),
        )?;
        let text = hover["contents"]["value"].as_str().unwrap_or_default();
        Ok(text.lines().next().unwrap_or_default().to_string())
    }
}

fn find_symbol<'a>(
    symbols: impl Iterator<Item = &'a Value>,
    position: Position,
    in_trait_impl: bool,
) -> Option<(&'a Value, bool)> {
    for symbol in symbols {
        if Position::from_lsp(&symbol["selectionRange"]["start"]) == position {
            return Some((symbol, in_trait_impl));
        }
        let name = symbol["name"].as_str().unwrap_or_default();
        let is_trait_impl = name.starts_with("impl") && name.contains(" for ");
        if let Some(found) = find_symbol(children(symbol), position, is_trait_impl) {
            return Some(found);
        }
    }
    None
}

fn children(symbol: &Value) -> impl Iterator<Item = &Value> {
    symbol["children"].as_array().into_iter().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notation_takes_a_package_name_for_the_crate() {
        let notation = Notation::parse("wado-compiler::name::Resolver").unwrap();
        assert_eq!(notation.path(), "wado_compiler::name::Resolver");
        assert_eq!(notation.name(), "Resolver");
        assert_eq!(notation.parent().unwrap().path(), "wado_compiler::name");
    }

    #[test]
    fn notation_rejects_what_is_not_a_path() {
        assert!(Notation::parse("probe").is_err());
        assert!(Notation::parse("probe::").is_err());
        assert!(Notation::parse("<probe::Foo as Shape>::area").is_err());
        assert!(Notation::parse("probe::Foo<T>::new").is_err());
    }
}
