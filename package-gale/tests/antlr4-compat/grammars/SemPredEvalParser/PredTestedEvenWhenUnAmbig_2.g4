grammar T;
@parser::members {boolean enumKeyword = true;}
primary
    :   ID {System.out.println("ID " + $ID.text);}
    |   {!(this.enumKeyword)}? 'enum' {System.out.println("enum");}
    ;
ID : [a-z]+ ;
WS : [ \t\n\r]+ -> skip ;
