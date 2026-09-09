// A regression for the FOLLOW-disjoint at-end-conflict refinement, on the
// `UNION` / `UNION ALL` shape in the `SQLite.g4` beside this file: MIT,
// Copyright (c) 2014 by Bart Kiers, https://github.com/bkiers/sqlite-parser.
// The rules below are written here.
//
// x has an at-end conflict ('a' 'b' ends while 'a' 'b' 'c' continues), but
// the divergence token 'c' cannot continue the caller (FOLLOW(x) = {'d'}),
// so the longest-match tournament is sound and the parser carries no ATN.
grammar LlAtEndFollowDisjoint;

s : x 'd' ;
x : 'a' 'b'
  | 'a' 'b' 'c'
  ;
WS : ' ' -> skip ;
