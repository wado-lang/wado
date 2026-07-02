grammar G;

root
    : {0==0}? continue+ {System.out.print($text);}
    ;

continue returns [int return]
    : for for? {1==1}?              #else
    | break=BREAK BREAK+ (for | IF) #else
    | if+=IF  if+=IF*               #int
    | continue CONTINUE_ {$return = 0;}   #class
    ;

args[int else] locals [int return]
    : for
    ;

for: FOR;
FOR: 'for ';
BREAK: 'break ';
IF: 'if ';
CONTINUE_: 'continue';
