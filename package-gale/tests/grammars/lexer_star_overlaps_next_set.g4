// Source: https://github.com/antlr/antlr4/issues/4813
// License: none stated in the report.
//
// In `'[' [ \t]* [xX ] [ \t]* ']'` the loop's set overlaps
// the set after it, so on `[ ]` the loop must leave the space for `[xX ]`.
grammar LexerStarOverlapsNextSet;

checklistFile : itemLine EOF ;
itemLine : ITEM_LINE_CONTENT NEWLINE+ ;

WS : [ \t]+ -> channel(HIDDEN) ;
ITEM_LINE_CONTENT : '-' [ \t]* '[' [ \t]* [xX ] [ \t]* ']' ( [ \t]+ ~[\r\n]+ )? ;
NEWLINE : ( '\r'? '\n' | '\r' )+ ;
ENDOFLIST : '---ENDOFLIST---' ;
