// Source: hand-written regression for the FOLLOW-disjoint at-end-conflict
// refinement (the SQLite `UNION` / `UNION ALL` shape).
// License: BSD-3-Clause (matches the rest of the gale test corpus).
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
