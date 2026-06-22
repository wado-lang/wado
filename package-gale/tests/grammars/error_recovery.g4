// Source: hand-written Gale regression fixture for parse-error positioning.
// License: same terms as the Gale package (see package-gale/README.md).
//
// Exercises two recovery paths:
//
//  * `prog : item* EOF` — a scan-gated repetition. When an item starts validly
//    but is malformed deeper in, the scan fails; the parser re-parses the stuck
//    item under a *speculative* probe to surface the precise inner error at the
//    malformed token (not the following construct), but the probe never commits
//    edits — static scan cannot tell a malformed item from a valid trailing
//    construct, so a folded tree plus one diagnostic is the safe result.
//
//  * `single : item EOF` / `double : item item EOF` — *mandatory* item calls.
//    Here recovery is committed: a missing terminal is inserted, a spurious one
//    deleted, an unrecoverable run swept into an `<error>` region, and the edits
//    land in the tree (the position is unambiguous, unlike a scan-gated loop).

grammar ErrorRecovery;

prog   : item* EOF ;
single : item EOF ;
double : item item EOF ;
item   : 'let' ID '=' ID ';'
       | block
       ;
block  : '{' item* '}' ;

ID : [a-zA-Z_] [a-zA-Z0-9_]* ;
WS : [ \t\r\n]+ -> skip ;
