// A regression for the scan-side optional rewind and the nullable-repeat walk,
// on the `table_or_subquery` / `FROM f(1)` shape in the `SQLite.g4` beside this
// file: MIT, Copyright (c) 2014 by Bart Kiers,
// https://github.com/bkiers/sqlite-parser. The rules below are written here.
//
// `item`'s alts share a rule ref and a '(' chain deeper than the static walk
// can separate, so the decision lands on the longest-match tournament. On
// `f ( ( ( ( 1 ) ) ) )` the optional `( 'as'? alias )?` is entered on '(' and
// then fails inside `alias`, with a trailing group reading the next token.
grammar ScanOptionalLookaheadRestore;

s : stmt EOF ;

// Forces a scan of `item` to the end — the call site that reached the trap.
stmt
 : item
 | item 'z'
 ;

item
 : name ( 'as'? alias )? ( 'idx' name | 'no' 'idx' )?
 | name '(' inner ')'
 ;

alias
 : ID
 | '(' alias ')'
 ;

inner
 : NUMBER
 | '(' inner ')'
 ;

name : ID ;

ID : [a-z]+ ;
NUMBER : [0-9]+ ;
WS : [ \t\r\n]+ -> skip ;
