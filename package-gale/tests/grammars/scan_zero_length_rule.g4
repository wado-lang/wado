// Source: hand-written regression for the zero-length rule scan (the SQLite
// `sql_stmt_list` / trailing `';'` shape).
// License: BSD-3-Clause (matches the rest of the gale test corpus).
//
// `stmt`'s mandatory alt group is emitted lenient, so scanning it where no
// alternative matches rewinds to the entry position and reports success at
// zero tokens. The `( ';'+ stmt )*` loop gate only checks that the body made
// progress, which the `';'` alone satisfies — so on `a ;` the loop enters and
// appends an empty `stmt`, and the trailing `';'*` never runs.
grammar ScanZeroLengthRule;

list : ';'* stmt ( ';'+ stmt )* ';'* EOF ;

stmt : 'k'? ( sa | sb ) ;

sa : 'a' ;

sb : 'b' ;

WS : [ \t\r\n]+ -> skip ;
