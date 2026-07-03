grammar ZzDedupProbe;
options { language = Wado; }
r : E e { p.emit(`v={$e.v}`); } ;
e returns [i32 v] : N { $v = 7; } ;
E : 'e' ;
N : 'n' ;
WS : ' ' -> skip ;
