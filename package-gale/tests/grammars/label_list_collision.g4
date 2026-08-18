// Regression: bindings that lower names in a nested scope but emit
// generates into the surrounding alternative's scope. Each rule pairs an
// author-written label with a construct whose natural name is the same, so a
// scope leak shows up as a duplicate `let` and the generated module simply
// does not compile.
//
// - `stream` — a list label appearing twice in the same alt, once outside any
//   Repeat and once wrapped in `*`. Each occurrence must emit a distinct
//   container, and the per-iteration parse value must not shadow either.
//   Mirrors the `if += IF  if += IF*` shape in
//   `ParserExec/ReservedWordsEscaping` without the Wado-keyword rename.
// - `leading_list` / `trailing_list` — a non-Repeat list label next to a label
//   that wants the list element's own natural name, in both orders.
// - `list_group` — the same, with a group as the list label's inner.
// - `nested_list` — a list label whose block holds another labelled element;
//   the surface walker used to accumulate it by hand instead of the builder.
// - `lazy_plus` / `lazy_plus_after` — a label next to a non-greedy `+`, whose
//   mandatory first iteration runs before the loop and so cannot be emitted
//   inside it. Both orders: emit can rebind against a name the alternative has
//   already bound, but not against one it binds later, so the iteration needs
//   a scope of its own either way.

grammar label_list_collision;

stream
    : tag += TAG  tag += TAG* EOF
    ;

leading_list
    : items += TAG  tag = TAG EOF
    ;

trailing_list
    : tag = TAG  items += TAG EOF
    ;

list_group
    : tag_or_word = TAG  items += (TAG | WORD) EOF
    ;

nested_list
    : outer += ( inner += TAG ) EOF
    ;

lazy_plus
    : tag = TAG  (TAG WORD)+? END EOF
    ;

lazy_plus_after
    : (TAG WORD)+? END  tag = TAG EOF
    ;

lazy_plus_list_after
    : items += (TAG WORD)+? END  tag = TAG EOF
    ;

TAG  : 'tag' ;
WORD : 'word' ;
END  : 'end' ;
WS   : [ \t\r\n]+ -> skip ;
