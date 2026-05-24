// Source: Hand-authored for Gale's parser-label coverage.
// License: BSD-3-Clause (matches the Gale repo).
//
// Two regressions packed into the same alt:
//
//   - `tag += TAG`  (Label without an enclosing Repeat): emits
//     `let mut tag: Array<Token> = []; let <temp> = p.expect(TK_TAG)?;
//      tag.push(<temp>);` in the alt's outer scope. The per-iter `<temp>`
//      sits in the same scope as the container `tag` AND defaults to
//      the inner TokenRef's field name (`tag`), so without an emit-side
//      rename the let would shadow the array.
//
//   - `tag += TAG*` (Repeat wrapping a list label) immediately after
//     the first labelled element: the Repeat's container picks a
//     deduplicated name (`tag_2`) and its loop body must not collide
//     with the alt-scope `<temp>` emitted by the first element.
//
// Reproduces the Gale codegen shape that fails inside
// `ParserExec/ReservedWordsEscaping.g4` (`if += IF  if += IF*`), minus
// the Wado-keyword rule rename that grammar adds on top.

grammar label_list_collision;

stream
    : tag += TAG  tag += TAG* EOF
    ;

TAG : 'tag' ;
WS  : [ \t\r\n]+ -> skip ;
