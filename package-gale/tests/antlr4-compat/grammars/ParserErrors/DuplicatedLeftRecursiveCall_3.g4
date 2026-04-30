grammar T;
start : expr EOF;
expr : 'x'
     | expr expr
     ;
