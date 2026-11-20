//! The bare names in a document's patterns that name a case or a constant, each
//! with the spelling that names it by its shape alone: `null` for the prelude's
//! `None`, `_::Case` for another case, `== NAME` for a global.

use wado_compiler::ast::{AstVisitor, Pattern, walk_item, walk_pattern};
use wado_compiler::symbol::SymbolKind;

use crate::query::QueryContext;

/// Replace the source bytes `start..end`, which spell `name`, with
/// `replacement`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternNameRewrite {
    pub start: usize,
    pub end: usize,
    pub name: String,
    pub replacement: String,
}

pub(crate) fn rewrites(ctx: &QueryContext) -> Vec<PatternNameRewrite> {
    let Some(ast) = ctx.sem.modules.get(ctx.entry()) else {
        return Vec::new();
    };
    let mut collector = Collector {
        ctx,
        rewrites: Vec::new(),
    };
    for item in &ast.items {
        walk_item(&mut collector, item);
    }
    collector.rewrites.sort_by_key(|r| r.start);
    collector.rewrites.dedup();
    collector.rewrites
}

struct Collector<'a, 'b> {
    ctx: &'a QueryContext<'b>,
    rewrites: Vec<PatternNameRewrite>,
}

impl Collector<'_, '_> {
    fn replacement(&self, id: wado_compiler::ast::AstId, name: &str) -> Option<String> {
        let sem = self.ctx.sem;
        if sem.local_type(id).is_some() {
            return None;
        }
        let def = sem.referenced_symbol(id)?;
        if matches!(sem.symbol_at(def).map(|s| &s.kind), Some(SymbolKind::Global(_))) {
            return Some(format!("== {name}"));
        }
        if name == "None" && sem.module_of_id(def).is_some_and(|m| m.is_prelude()) {
            return Some("null".to_string());
        }
        Some(format!("_::{name}"))
    }
}

impl<'ast> AstVisitor<'ast> for Collector<'_, '_> {
    fn visit_pattern(&mut self, pat: &'ast Pattern) {
        match pat {
            // A range bound and an `==` operand never bind, so they keep
            // their spelling.
            Pattern::Range { .. } | Pattern::Equal { .. } => return,
            Pattern::Ident { id, name, span } => {
                if let Some(replacement) = self.replacement(*id, name) {
                    self.rewrites.push(PatternNameRewrite {
                        start: span.start,
                        end: span.end,
                        name: name.clone(),
                        replacement,
                    });
                }
            }
            _ => {}
        }
        walk_pattern(self, pat);
    }
}
