// Source: Gale test fixture (Stage A $ctx.toStringTree)
// License: same as the Gale package
//
// `$ctx.toStringTree()` renders the S-expression of the rule node currently
// under construction. In `s`'s `@after` the `s` node is still open with its
// children (`e` subtree, EOF) complete, so the render equals the finalized
// tree's root rendering.
grammar WadoCtxTree;

options { language = Wado; }

s @after { p.emit($ctx.toStringTree()); } : e EOF ;
e : A B ;

A : 'a' ;
B : 'b' ;
WS : ' ' -> skip ;
