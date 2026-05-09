// Comment handling for Wado formatter
//
// Two complementary stores:
//
// - `CommentMap` is keyed by source byte position. Used by leaf paths in the
//   unparser that look up "any comment in this byte range" (file-tail
//   dangling, comments on closing braces, etc.).
//
// - `TriviaMap` is keyed by `AstId`. Populated by the parser as it allocates
//   ids: every comment that appears in source between the previous token
//   stream position and the next token of the node being constructed is
//   attached to that node's id as *leading trivia*. The unparser reads
//   `trivia.leading_of(id)` immediately before emitting the node, so an
//   inline `/*name=*/` comment ends up exactly where it semantically
//   belongs — attached to the AST node it precedes — without any
//   position-based reconstruction.
//
// `TriviaMap` is the primary mechanism for new code. `CommentMap` continues
// to back the residual position-based paths until they are migrated.

use crate::ast::AstId;
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

/// Maps byte positions to comments.
/// Uses `BTreeMap` for ordered iteration (important for unparsing).
#[derive(Debug, Clone, Default)]
pub struct CommentMap {
    /// Comments indexed by their start byte position
    comments: BTreeMap<usize, Comment>,
    /// Source line count for position calculations
    line_starts: Vec<usize>,
}

impl CommentMap {
    /// Create an empty comment map
    pub fn new() -> Self {
        Self {
            comments: BTreeMap::new(),
            line_starts: Vec::new(),
        }
    }

    /// Create a comment map from a list of comments
    pub fn from_comments(comments: Vec<Comment>, source: &str) -> Self {
        let mut map = Self::new();
        map.build_line_starts(source);
        for comment in comments {
            map.insert(comment);
        }
        map
    }

    /// Build line start positions from source
    fn build_line_starts(&mut self, source: &str) {
        self.line_starts.clear();
        self.line_starts.push(0); // Line 1 starts at position 0
        for (i, ch) in source.char_indices() {
            if ch == '\n' {
                self.line_starts.push(i + 1);
            }
        }
    }

    /// Add a comment at its position
    pub fn insert(&mut self, comment: Comment) {
        self.comments.insert(comment.span.start, comment);
    }

    /// Get comments in a byte range [start, end)
    pub fn comments_in_range(&self, start: usize, end: usize) -> Vec<&Comment> {
        self.comments.range(start..end).map(|(_, c)| c).collect()
    }

    /// Get leading comments for a span.
    /// Leading comments are comments that appear before the span starts,
    /// either on the same line or on immediately preceding lines.
    pub fn leading_comments(&self, span: &Span) -> Vec<&Comment> {
        // Get all comments that end before this span starts
        self.comments
            .range(..span.start)
            .map(|(_, c)| c)
            .filter(|c| {
                // Comment must be on a line before or the same line as the span
                c.span.line <= span.line
            })
            .collect()
    }

    /// Get trailing comments for a span.
    /// Trailing comments are comments on the same line as the span's end position.
    pub fn trailing_comments(&self, span: &Span) -> Vec<&Comment> {
        self.comments
            .range(span.end..)
            .map(|(_, c)| c)
            .take_while(|c| c.span.line == span.end_line())
            .collect()
    }

    /// Iterate all comments in order by position
    pub fn iter(&self) -> impl Iterator<Item = &Comment> {
        self.comments.values()
    }

    /// Check if the map is empty
    pub fn is_empty(&self) -> bool {
        self.comments.is_empty()
    }

    /// Get the number of comments
    pub fn len(&self) -> usize {
        self.comments.len()
    }

    /// Get a comment at a specific position
    pub fn get(&self, pos: usize) -> Option<&Comment> {
        self.comments.get(&pos)
    }

    /// Remove and return a comment at a specific position
    pub fn remove(&mut self, pos: usize) -> Option<Comment> {
        self.comments.remove(&pos)
    }

    /// Get comments between two positions (exclusive), typically used for
    /// finding comments between consecutive AST nodes
    pub fn comments_between(&self, after: usize, before: usize) -> Vec<&Comment> {
        if after >= before {
            return Vec::new();
        }
        self.comments.range(after..before).map(|(_, c)| c).collect()
    }

    /// Count blank lines between two line numbers.
    /// Returns the number of consecutive empty lines between end of `prev_line` and start of `next_line`.
    /// Rule: 0 → 0, 1 → 1, 2+ → 2
    pub fn blank_lines_between(&self, prev_line: usize, next_line: usize) -> usize {
        if next_line <= prev_line + 1 {
            return 0;
        }
        let blank_count = next_line - prev_line - 1;
        match blank_count {
            0 => 0,
            1 => 1,
            _ => 2,
        }
    }
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
/// `dangling` collects comments that fall after the last token of the
/// last allocated AST node — typically a comment on a trailing line of
/// the file. Those are emitted by the unparser at a fixed sink (the end
/// of the module) without an AST anchor.
#[derive(Debug, Clone, Default)]
pub struct TriviaMap {
    leading: BTreeMap<AstId, Vec<Comment>>,
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

    /// File-tail dangling comments (after the last AST node).
    pub fn dangling(&self) -> &[Comment] {
        &self.dangling
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_comment(text: &str, start: usize, end: usize, line: usize) -> Comment {
        Comment {
            text: text.to_string(),
            kind: CommentKind::Line,
            span: Span::new(start, end, line, 1),
        }
    }

    #[test]
    fn test_comment_map_insert_and_iter() {
        let mut map = CommentMap::new();
        map.insert(make_comment(" comment 1", 0, 12, 1));
        map.insert(make_comment(" comment 2", 20, 32, 2));

        let comments: Vec<_> = map.iter().collect();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].text, " comment 1");
        assert_eq!(comments[1].text, " comment 2");
    }

    #[test]
    fn test_comment_map_range() {
        let mut map = CommentMap::new();
        map.insert(make_comment(" c1", 0, 5, 1));
        map.insert(make_comment(" c2", 10, 15, 2));
        map.insert(make_comment(" c3", 20, 25, 3));

        let comments = map.comments_in_range(5, 20);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].text, " c2");
    }

    #[test]
    fn test_comment_map_between() {
        let mut map = CommentMap::new();
        map.insert(make_comment(" before", 0, 10, 1));
        map.insert(make_comment(" middle", 15, 25, 2));
        map.insert(make_comment(" after", 30, 40, 3));

        let comments = map.comments_between(10, 30);
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].text, " middle");
    }

    #[test]
    fn test_comment_map_trailing() {
        let mut map = CommentMap::new();
        // Comment on line 1 at position 20 (after code at position 0-15)
        map.insert(Comment {
            text: " trailing".to_string(),
            kind: CommentKind::Line,
            span: Span::new(20, 32, 1, 21),
        });
        // Comment on line 2
        map.insert(Comment {
            text: " next line".to_string(),
            kind: CommentKind::Line,
            span: Span::new(40, 52, 2, 1),
        });

        // Code span on line 1, ending at position 15
        let code_span = Span::new(0, 15, 1, 1);
        let trailing = map.trailing_comments(&code_span);

        assert_eq!(trailing.len(), 1);
        assert_eq!(trailing[0].text, " trailing");
    }

    #[test]
    fn test_from_comments() {
        let source = "// comment 1\nlet x = 1; // comment 2\n";
        let comments = vec![
            make_comment(" comment 1", 0, 12, 1),
            make_comment(" comment 2", 24, 36, 2),
        ];

        let map = CommentMap::from_comments(comments, source);
        assert_eq!(map.len(), 2);
    }
}
