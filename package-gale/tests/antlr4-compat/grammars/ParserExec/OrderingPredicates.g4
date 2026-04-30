grammar Issue2301;

SPACES: [ \t\r\n]+ -> skip;

AT: 'AT';
X : 'X';
Y : 'Y';

ID: [A-Z]+;

constant
: 'DUMMY'
;

expr
: ID constant?
| expr AT X
| expr AT Y
;
