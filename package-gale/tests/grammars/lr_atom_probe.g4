// The shape of `expr : … | K_CASE expr? (K_WHEN expr K_THEN expr)+ … K_END` in
// the `SQLite.g4` beside this file: MIT, Copyright (c) 2014 by Bart Kiers,
// https://github.com/bkiers/sqlite-parser. The rules below are written here.
//
// A viability probe inside an LR rule's ATOM alternative. Keywords double as
// identifiers, so the optional's body and its continuation share a first
// token and the probe decides the entry. The atom ends where the precedence
// loop begins, so the probe must stop at the atom — asking the caller's
// FOLLOW there reads a loop-continuing `+` as unviable. `tail` is what turns
// the runtime FOLLOW gate on for the grammar, and the parentheses are what
// give `e` a non-empty FOLLOW to be asked.
grammar LrAtomProbe;

s : '(' e ')' tail EOF ;

tail : 'z'* ;

e
    : e '+' e
    | 'case' e? ('when' e 'then' e)+ 'end'
    | name
    ;

name : ID | 'when' | 'then' | 'end' | 'case' ;

ID : [a-z]+ ;
WS : [ \t\r\n]+ -> skip ;
