// A regression for the zero-length rule scan, on the `sql_stmt_list` /
// trailing `';'` shape in the `SQLite.g4` beside this file: MIT,
// Copyright (c) 2014 by Bart Kiers, https://github.com/bkiers/sqlite-parser.
// The rules below are written here.
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
