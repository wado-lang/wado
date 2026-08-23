// The Java twin of `impl RustParserBase for RustPBase` in
// package-gale/tests/driver_cst_rust_test.wado, so `antlr4-oracle.sh --super`
// runs ANTLR4 against the base class Gale runs against. Edit both or neither.
import org.antlr.v4.runtime.Parser;
import org.antlr.v4.runtime.TokenStream;

public abstract class RustParserBase extends Parser {
    public RustParserBase(TokenStream input) {
        super(input);
    }

    // `<` and `>` are one character wide, so an adjacent pair — a shift
    // operator rather than two comparisons — is one start offset apart.
    private boolean adjacent() {
        return _input.LT(1).getStartIndex() - _input.LT(-1).getStartIndex() == 1;
    }

    public boolean NextLT() {
        return adjacent();
    }

    public boolean NextGT() {
        return adjacent();
    }
}
