// Regression: a list label appearing twice in the same alt — once
// outside any Repeat, once wrapped in `*`. Each occurrence must emit
// a distinct container, and the per-iteration parse value must not
// shadow either container. Mirrors the
// `if += IF  if += IF*` shape in `ParserExec/ReservedWordsEscaping`
// without the Wado-keyword rename.

grammar label_list_collision;

stream
    : tag += TAG  tag += TAG* EOF
    ;

TAG : 'tag' ;
WS  : [ \t\r\n]+ -> skip ;
