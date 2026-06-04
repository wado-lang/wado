// Source: hand-written Gale regression fixture for parse-error positioning.
// License: same terms as the Gale package (see package-gale/README.md).
//
// Exercises the "error points at the malformed element, not the next
// construct" behaviour. A repetition (`item*`) is gated by a scan; when an
// item starts validly but is malformed deeper in, the scan fails and the loop
// exits. Without recovery the parser then blames the following required token
// (`}` / EOF) at the *next* item. The generated parser instead re-parses the
// stuck item on the failure path to surface the precise inner error.

grammar ErrRec;

prog  : item* EOF ;
item  : 'let' ID '=' ID ';'
      | block
      ;
block : '{' item* '}' ;

ID : [a-zA-Z_] [a-zA-Z0-9_]* ;
WS : [ \t\r\n]+ -> skip ;
