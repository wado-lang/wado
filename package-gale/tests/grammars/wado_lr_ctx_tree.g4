// Source: Gale test fixture (Stage A $ctx.toStringTree on an LR rule)
// License: same as the Gale package
//
// `$ctx.toStringTree()` in the `@after` of a LEFT-RECURSIVE rule. At exit the
// LR node is already closed (precedence climbing folds each level), so the
// render must come from the most-recently-finished node, not the open stack.
grammar WadoLrCtxTree;

options { language = Wado; }

e @after { p.emit(`[${$ctx.toStringTree()}]`); } : e '+' e | INT ;

INT : [0-9]+ ;
WS : ' ' -> skip ;
