grammar Test;
import Unicode;

program : 'test' 'test';

WS : (UNICODE_CLASS_Zs)+ -> skip;
