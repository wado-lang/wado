// Comment handling for Wado formatter and `wado doc`.
//
// `TriviaMap` is the single AstId-keyed store for comments:
//   * `leading_of(id)` — comments that precede the node, populated by
//     the parser on each `alloc_ast_id` call.
//   * `trailing_of(id)` — same-line tail comments, populated by
//     [`populate_trailing`] after parsing completes.
//   * `inner_tail_of(block_id)` — comments between a block's last
//     statement and its closing `}`, populated by
//     [`populate_inner_tail`].
//   * `dangling()` — file-tail comments past the last AST node.
//
// `populate_trailing` and `populate_inner_tail` walk the parsed AST and
// repatriate comments out of their parser-assigned slots; the unparser
// then reads `TriviaMap` exclusively, with no positional reconstruction.

use crate::ast::{AstId, AstVisitor, Block, Module, walk_block};
use crate::token::Span;
use std::collections::BTreeMap;

/// A comment in the source code
#[derive(Debug, Clone)]
pub struct Comment {
    /// The comment text (without delimiters like `//` or `/* */`)
    pub text: String,
    /// The kind of comment
    pub kind: CommentKind,
    /// Source location
    pub span: Span,
}

/// The kind of comment
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentKind {
    /// Line comment: `// ...`
    Line,
    /// Block comment: `/* ... */`
    Block,
    /// Doc comment: `/// ...`
    DocLine,
    /// Module doc comment: `//! ...`
    ModuleDoc,
}

/// Trivia (currently: comments) attached to AST nodes by [`AstId`].
///
/// The parser populates this as it allocates ids: at each `alloc_ast_id`
/// call, every still-unattached comment whose `span.start` precedes the
/// next-to-consume token's start is attached to the new id as leading
/// trivia. Because `alloc_ast_id` is called in DFS parse order — the
/// outermost node being constructed at a given source position allocates
/// first — leading comments naturally land on the outermost AST node that
/// "starts" at that position, which matches the semantic ownership users
/// expect (`/*flag=*/true` belongs to the literal `true`, not to the
/// surrounding call).
///
/// `trailing` is populated by [`populate_trailing`] in a post-parse pass:
/// any comment that sits on the same source line as a node's end position
/// and starts at or after that position is repatriated from its
/// parser-assigned `leading`/`dangling` slot to `trailing[node]`. The
/// unparser reads `trailing_of(id)` to emit "tail" comments inline with
/// the closing token of the node they belong to.
///
/// `inner_tail` is populated by [`populate_inner_tail`]: comments that
/// fall *between the end of a block's last statement and its closing
/// brace* are attributed to the innermost containing block's id. The
/// unparser flushes them just before emitting the `}` of that block.
///
/// `dangling` collects comments that fall after the last token of the
/// last allocated AST node — typically a comment on a trailing line of
/// the file. Those are emitted by the unparser at a fixed sink (the end
/// of the module) without an AST anchor.
#[derive(Debug, Clone, Default)]
pub struct TriviaMap {
    leading: BTreeMap<AstId, Vec<Comment>>,
    trailing: BTreeMap<AstId, Vec<Comment>>,
    inner_tail: BTreeMap<AstId, Vec<Comment>>,
    dangling: Vec<Comment>,
}

impl TriviaMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach `comments` as leading trivia on `id`. Idempotent within a
    /// single parse — the parser only calls this once per id.
    pub fn attach_leading(&mut self, id: AstId, comments: Vec<Comment>) {
        if comments.is_empty() {
            return;
        }
        debug_assert!(
            !self.leading.contains_key(&id),
            "TriviaMap::attach_leading called twice for {id:?}",
        );
        self.leading.insert(id, comments);
    }

    /// Set the file-tail dangling comments — those that appear in source
    /// after the last AST node's end. Called once at parse end.
    pub fn set_dangling(&mut self, comments: Vec<Comment>) {
        self.dangling = comments;
    }

    /// Leading trivia for `id`, or an empty slice if none.
    pub fn leading_of(&self, id: AstId) -> &[Comment] {
        self.leading.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Trailing trivia for `id`, or an empty slice if none. Populated by
    /// [`populate_trailing`].
    pub fn trailing_of(&self, id: AstId) -> &[Comment] {
        self.trailing.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Inner-tail trivia for a block `id`, or an empty slice if none.
    /// Populated by [`populate_inner_tail`].
    pub fn inner_tail_of(&self, id: AstId) -> &[Comment] {
        self.inner_tail.get(&id).map(Vec::as_slice).unwrap_or(&[])
    }

    /// File-tail dangling comments (after the last AST node).
    pub fn dangling(&self) -> &[Comment] {
        &self.dangling
    }

    /// Drop every leading-trivia entry whose key is `>= threshold`.
    /// Used by the parser when a speculative branch backtracks: the
    /// AST ids it allocated are about to be re-issued, so any trivia
    /// keyed by them must be cleared first. `trailing` is not pruned
    /// here because it is populated only after parsing completes, so
    /// no speculative ids can be in it.
    pub fn discard_from(&mut self, threshold: AstId) {
        self.leading.retain(|id, _| *id < threshold);
    }
}

/// Per-line index over a node table, used by [`trailing_owner`] so each
/// per-comment lookup is bounded by the number of nodes ending or
/// starting on the comment's line — not the whole module's node count.
struct NodeLookup {
    /// `(AstId, Span)` for each node, grouped by `span.end_line()`.
    /// The bucket for a line holds every node whose span ends on that
    /// line; `trailing_owner`'s candidate filter only needs to scan
    /// this bucket.
    by_end_line: BTreeMap<usize, Vec<(AstId, Span)>>,
    /// `span.start` byte positions, grouped by the node's start line
    /// and sorted ascending. Used to binary-search the
    /// "is there a node starting after `c.span.end` on `c`'s end
    /// line?" disqualification check.
    starts_by_line: BTreeMap<usize, Vec<usize>>,
}

impl NodeLookup {
    fn build(nodes: &[(AstId, Span)]) -> Self {
        let mut by_end_line: BTreeMap<usize, Vec<(AstId, Span)>> = BTreeMap::new();
        let mut starts_by_line: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (id, span) in nodes {
            by_end_line.entry(span.end_line()).or_default().push((*id, *span));
            starts_by_line.entry(span.line).or_default().push(span.start);
        }
        for v in starts_by_line.values_mut() {
            v.sort_unstable();
        }
        Self {
            by_end_line,
            starts_by_line,
        }
    }

    /// See [`trailing_owner`]; same algorithm using the indexed buckets.
    fn trailing_owner(&self, c: &Comment) -> Option<AstId> {
        if let Some(starts) = self.starts_by_line.get(&c.span.end_line()) {
            // Sorted ascending; if any value is >= c.span.end, the
            // comment is interior trivia — see `trailing_owner` docs.
            let cut = starts.partition_point(|&s| s < c.span.end);
            if cut < starts.len() {
                return None;
            }
        }
        let bucket = self.by_end_line.get(&c.span.line)?;
        bucket
            .iter()
            .filter(|(_, s)| s.end <= c.span.start)
            .max_by_key(|(_, s)| (s.end, std::cmp::Reverse(s.start)))
            .map(|(id, _)| *id)
    }
}

// `trailing_owner` is a method on [`NodeLookup`] now; the production
// path in [`populate_trailing_from_nodes`] reuses a single lookup
// across every comment instead of rebuilding it. Unit tests below
// keep a one-shot wrapper for readability.

/// Collect `(AstId, Span)` for every id-bearing node reached from `module`.
fn collect_node_spans(module: &Module) -> Vec<(AstId, Span)> {
    struct Collector(Vec<(AstId, Span)>);
    impl AstVisitor for Collector {
        fn visit_id(&mut self, id: AstId, span: Span) {
            self.0.push((id, span));
        }
    }
    let mut c = Collector(Vec::new());
    for item in &module.items {
        c.visit_item(item);
    }
    c.0
}

/// Repatriate same-line trailing comments out of `leading[*]` and
/// `dangling` into `trailing[owner]`, using the [`trailing_owner`]
/// rule. Idempotent: a second call is a no-op because the surviving
/// `leading`/`dangling` entries by definition have no trailing owner.
pub fn populate_trailing(trivia: &mut TriviaMap, module: &Module) {
    let nodes = collect_node_spans(module);
    populate_trailing_from_nodes(trivia, &nodes);
}

/// Identify the innermost block whose interior — between the last
/// statement's end and the closing `}` — contains `c`. Returns `None`
/// if `c` is not inside any block's interior. Innermost = smallest end
/// position among the candidate set.
fn inner_tail_owner(c: &Comment, blocks: &[(AstId, usize, usize)]) -> Option<AstId> {
    blocks
        .iter()
        .filter(|(_, after, end)| *after <= c.span.start && c.span.start < *end)
        .min_by_key(|(_, _, end)| *end)
        .map(|(id, _, _)| *id)
}

/// Collect `(block.id, after, end)` for every [`Block`] in `module`,
/// where `after` is the byte position immediately after the block's
/// last statement (or `block.span.start` for an empty block) and `end`
/// is `block.span.end`. The interval `[after, end)` is the block's
/// "inner-tail" range — the source bytes between its content and its
/// closing brace.
fn collect_block_ranges(module: &Module) -> Vec<(AstId, usize, usize)> {
    struct Collector(Vec<(AstId, usize, usize)>);
    impl AstVisitor for Collector {
        fn visit_block(&mut self, b: &Block) {
            let after = b.stmts.last().map_or(b.span.start, |last| last.span().end);
            self.0.push((b.id, after, b.span.end));
            walk_block(self, b);
        }
    }
    let mut c = Collector(Vec::new());
    for item in &module.items {
        c.visit_item(item);
    }
    c.0
}

/// Repatriate comments that sit between a block's last statement and
/// its closing `}` into `inner_tail[block.id]`. Innermost block wins
/// for nested cases. Idempotent: the surviving `leading`/`dangling`
/// entries by definition contain no comments inside any block's
/// inner-tail range.
///
/// Run *after* [`populate_trailing`] — comments already moved to a
/// trailing slot are no longer in `leading`/`dangling` and are
/// therefore left alone, preserving their stronger same-line tail
/// attribution.
pub fn populate_inner_tail(trivia: &mut TriviaMap, module: &Module) {
    let blocks = collect_block_ranges(module);
    populate_inner_tail_from_blocks(trivia, &blocks);
}

/// AST-shape-free core of [`populate_inner_tail`]. Exposed for unit
/// tests that construct synthetic block tables.
fn populate_inner_tail_from_blocks(trivia: &mut TriviaMap, blocks: &[(AstId, usize, usize)]) {
    let leading_ids: Vec<AstId> = trivia.leading.keys().copied().collect();
    for id in leading_ids {
        let comments = trivia.leading.remove(&id).expect("key was just listed");
        let mut stayers: Vec<Comment> = Vec::with_capacity(comments.len());
        for c in comments {
            match inner_tail_owner(&c, blocks) {
                Some(owner) => trivia.inner_tail.entry(owner).or_default().push(c),
                None => stayers.push(c),
            }
        }
        if !stayers.is_empty() {
            trivia.leading.insert(id, stayers);
        }
    }

    let dangling = std::mem::take(&mut trivia.dangling);
    let mut stayers: Vec<Comment> = Vec::with_capacity(dangling.len());
    for c in dangling {
        match inner_tail_owner(&c, blocks) {
            Some(owner) => trivia.inner_tail.entry(owner).or_default().push(c),
            None => stayers.push(c),
        }
    }
    trivia.dangling = stayers;
}

/// AST-shape-free core of [`populate_trailing`]. Exposed for unit tests
/// that construct synthetic `(AstId, Span)` tables.
fn populate_trailing_from_nodes(trivia: &mut TriviaMap, nodes: &[(AstId, Span)]) {
    let lookup = NodeLookup::build(nodes);
    let leading_ids: Vec<AstId> = trivia.leading.keys().copied().collect();
    for id in leading_ids {
        let comments = trivia.leading.remove(&id).expect("key was just listed");
        let mut stayers: Vec<Comment> = Vec::with_capacity(comments.len());
        for c in comments {
            match lookup.trailing_owner(&c) {
                Some(owner) => trivia.trailing.entry(owner).or_default().push(c),
                None => stayers.push(c),
            }
        }
        if !stayers.is_empty() {
            trivia.leading.insert(id, stayers);
        }
    }

    let dangling = std::mem::take(&mut trivia.dangling);
    let mut stayers: Vec<Comment> = Vec::with_capacity(dangling.len());
    for c in dangling {
        match lookup.trailing_owner(&c) {
            Some(owner) => trivia.trailing.entry(owner).or_default().push(c),
            None => stayers.push(c),
        }
    }
    trivia.dangling = stayers;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_comment(text: &str, start: usize, end: usize, line: usize, column: usize) -> Comment {
        Comment {
            text: text.to_string(),
            kind: CommentKind::Line,
            span: Span::new(start, end, line, column),
        }
    }

    /// Span helper that places start/end on the same line `line`. Used to
    /// build small synthetic node tables for the trailing-attribution
    /// tests; trailing attribution only inspects `span.end`/`span.line`,
    /// so the column values are irrelevant beyond their byte order.
    fn one_line_span(start: usize, end: usize, line: usize) -> Span {
        Span::new(start, end, line, 1)
    }

    #[test]
    fn populate_picks_outermost_when_nodes_tie_at_same_end() {
        // Three nodes end on the same line as a trailing comment.
        //   A: 0..10  (outer)
        //   B: 5..10  (inner, same end as A — outer wins on tiebreak)
        //   C: 0..8   (smaller end — never wins)
        // Without the (largest-end, smallest-start) rule the trailing
        // comment would land on B and the unparser — which only emits
        // trailing of stmt/item ids — would silently drop it.
        let nodes = vec![
            (AstId(0), one_line_span(0, 10, 1)),
            (AstId(1), one_line_span(5, 10, 1)),
            (AstId(2), one_line_span(0, 8, 1)),
        ];
        let mut trivia = TriviaMap::new();
        trivia.set_dangling(vec![line_comment(" tail", 12, 18, 1, 13)]);

        populate_trailing_from_nodes(&mut trivia, &nodes);

        assert_eq!(trivia.trailing_of(AstId(0)).len(), 1);
        assert!(trivia.trailing_of(AstId(1)).is_empty());
        assert!(trivia.trailing_of(AstId(2)).is_empty());
    }

    #[test]
    fn populate_keeps_interior_block_comment_as_leading_of_next() {
        // `1, /*flag=*/true` shape: the parser attaches the inline
        // block comment as leading of `true`. Repatriating it as
        // trailing of `1` would lose the named-argument hint, which
        // is why the trailing rule disqualifies any comment with a
        // following node on its end line. Pinned by the user-visible
        // `test_format_preserves_inline_block_comment_in_call_args`
        // formatter test as well; this is the algorithm-level guard.
        let one_lit = (AstId(0), Span::new(0, 1, 1, 1));
        let true_lit = (AstId(1), Span::new(12, 16, 1, 13));
        let nodes = vec![one_lit, true_lit];
        let mut trivia = TriviaMap::new();
        let inline = Comment {
            text: "flag=".to_string(),
            kind: CommentKind::Block,
            span: Span::new(3, 12, 1, 4),
        };
        trivia.attach_leading(AstId(1), vec![inline]);

        populate_trailing_from_nodes(&mut trivia, &nodes);

        assert!(
            trivia.trailing_of(AstId(0)).is_empty(),
            "interior block comment must not become trailing of the previous node",
        );
        assert_eq!(
            trivia.leading_of(AstId(1)).len(),
            1,
            "interior block comment must remain leading of the following node",
        );
    }

    #[test]
    fn populate_moves_from_dangling_to_trailing() {
        let nodes = vec![(AstId(0), one_line_span(0, 10, 1))];
        let mut trivia = TriviaMap::new();
        trivia.set_dangling(vec![line_comment(" tail", 12, 18, 1, 13)]);

        populate_trailing_from_nodes(&mut trivia, &nodes);

        assert_eq!(trivia.trailing_of(AstId(0)).len(), 1);
        assert_eq!(trivia.trailing_of(AstId(0))[0].text, " tail");
        assert!(trivia.dangling().is_empty());
    }

    #[test]
    fn populate_moves_from_leading_of_next_to_trailing_of_prev() {
        // Source-shape:  prev_node ends at byte 10, line 1.
        //                 // tail of prev    (line 1, byte 12)
        //                 next_node starts at line 2, byte 30.
        // Parser would attach the comment as leading of next_node;
        // populate_trailing must move it to trailing of prev_node.
        let nodes = vec![
            (AstId(0), one_line_span(0, 10, 1)),
            (AstId(1), Span::new(30, 40, 2, 1)),
        ];
        let mut trivia = TriviaMap::new();
        trivia.attach_leading(AstId(1), vec![line_comment(" tail", 12, 18, 1, 13)]);

        populate_trailing_from_nodes(&mut trivia, &nodes);

        assert_eq!(trivia.trailing_of(AstId(0)).len(), 1);
        assert_eq!(trivia.trailing_of(AstId(0))[0].text, " tail");
        assert!(
            trivia.leading_of(AstId(1)).is_empty(),
            "comment must be removed from the leading slot it was repatriated out of",
        );
    }

    #[test]
    fn populate_keeps_non_trailing_leading_in_place() {
        // Comment is a true header (different line from the node it
        // precedes), parser attaches it as leading; populate_trailing
        // should leave it alone.
        let nodes = vec![(AstId(0), Span::new(20, 30, 3, 1))];
        let mut trivia = TriviaMap::new();
        let header = line_comment(" header", 0, 8, 2, 1);
        trivia.attach_leading(AstId(0), vec![header]);

        populate_trailing_from_nodes(&mut trivia, &nodes);

        assert_eq!(trivia.leading_of(AstId(0)).len(), 1);
        assert_eq!(trivia.leading_of(AstId(0))[0].text, " header");
        assert!(trivia.trailing_of(AstId(0)).is_empty());
    }

    #[test]
    fn inner_tail_owner_picks_innermost_containing_block() {
        // Outer block: [10..100), inner block: [50..80). A comment at
        // byte 70 is contained by both — innermost (= smallest end)
        // should win.
        let blocks = vec![
            (AstId(1), 10, 100), // outer
            (AstId(2), 50, 80),  // inner
        ];
        let comment = line_comment(" inner-tail", 70, 84, 5, 1);
        assert_eq!(inner_tail_owner(&comment, &blocks), Some(AstId(2)));
    }

    #[test]
    fn populate_inner_tail_moves_dangling_in_block_to_inner_tail() {
        // Block id 0 spans [0..50), last stmt ends at byte 20.
        // A comment at byte 30 is inside the block's tail range.
        let blocks = vec![(AstId(0), 20, 50)];
        let mut trivia = TriviaMap::new();
        trivia.set_dangling(vec![line_comment(" tail", 30, 38, 3, 5)]);

        populate_inner_tail_from_blocks(&mut trivia, &blocks);

        assert_eq!(trivia.inner_tail_of(AstId(0)).len(), 1);
        assert_eq!(trivia.inner_tail_of(AstId(0))[0].text, " tail");
        assert!(trivia.dangling().is_empty());
    }

    #[test]
    fn populate_inner_tail_does_not_disturb_unrelated_leading() {
        // Leading comment of a node OUTSIDE any block's inner-tail
        // range stays as leading.
        let blocks = vec![(AstId(0), 100, 200)];
        let mut trivia = TriviaMap::new();
        trivia.attach_leading(AstId(99), vec![line_comment(" header", 0, 8, 1, 1)]);

        populate_inner_tail_from_blocks(&mut trivia, &blocks);

        assert_eq!(trivia.leading_of(AstId(99)).len(), 1);
        assert!(trivia.inner_tail_of(AstId(0)).is_empty());
    }

    #[test]
    fn populate_inner_tail_is_idempotent() {
        let blocks = vec![(AstId(0), 20, 50)];
        let mut trivia = TriviaMap::new();
        trivia.set_dangling(vec![line_comment(" tail", 30, 38, 3, 5)]);

        populate_inner_tail_from_blocks(&mut trivia, &blocks);
        let after_first: Vec<_> = trivia
            .inner_tail_of(AstId(0))
            .iter()
            .map(|c| c.text.clone())
            .collect();

        populate_inner_tail_from_blocks(&mut trivia, &blocks);
        let after_second: Vec<_> = trivia
            .inner_tail_of(AstId(0))
            .iter()
            .map(|c| c.text.clone())
            .collect();

        assert_eq!(after_first, after_second);
        assert_eq!(after_first.len(), 1);
    }

    #[test]
    fn populate_is_idempotent() {
        let nodes = vec![(AstId(0), one_line_span(0, 10, 1))];
        let mut trivia = TriviaMap::new();
        trivia.set_dangling(vec![line_comment(" tail", 12, 18, 1, 13)]);

        populate_trailing_from_nodes(&mut trivia, &nodes);
        let trailing_after_first = trivia.trailing_of(AstId(0)).to_vec();

        populate_trailing_from_nodes(&mut trivia, &nodes);
        let trailing_after_second = trivia.trailing_of(AstId(0)).to_vec();

        assert_eq!(trailing_after_first.len(), 1);
        assert_eq!(trailing_after_second.len(), 1);
        assert_eq!(
            trailing_after_first[0].text, trailing_after_second[0].text,
            "second call must not duplicate the comment",
        );
    }
}
