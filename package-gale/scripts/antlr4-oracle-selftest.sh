#!/usr/bin/env bash
# Exercise antlr4-oracle.sh against the corpus grammars. Not a CI job: it
# needs java, javac and the cached jar, the same as regen-oracle.sh. Run it
# after touching the oracle — its --stub-super soundness gate decides whether
# a generated fixture pins ANTLR4's answer or a stub's guess, and nothing
# else checks that.
#
# Usage: scripts/antlr4-oracle-selftest.sh
set -uo pipefail
cd "$(dirname "$0")/.."

export ANTLR4_VERSION="${ANTLR4_VERSION:-4.13.2}"
ORACLE="scripts/antlr4-oracle.sh"
RUST_BASE="tests/grammars/java/RustLexerBase.java"
FAILED=0

# check <name> <input> <rc> <stdout-substring> <stderr-substring> -- <oracle args...>
# An empty stdout-substring asserts stdout is empty: a refused run must not
# leave a caller anything to pin.
check() {
    local name="$1" input="$2" want_rc="$3" want_out="$4" want_err="$5"
    shift 6  # name, input, rc, out, err, --
    local out rc err
    err=$(mktemp)
    out=$(printf '%s' "$input" | bash "$ORACLE" "$@" 2>"$err")
    rc=$?
    if [ "$rc" != "$want_rc" ]; then
        echo "FAIL $name: exit $rc, want $want_rc" >&2
        FAILED=$((FAILED + 1))
        rm -f "$err"
        return
    fi
    if [ -n "$want_err" ] && ! grep -qF "$want_err" "$err"; then
        echo "FAIL $name: stderr lacks '$want_err'" >&2
        sed 's/^/  /' "$err" >&2
        FAILED=$((FAILED + 1))
        rm -f "$err"
        return
    fi
    rm -f "$err"
    if [ -n "$want_out" ] && [[ "$out" != *"$want_out"* ]]; then
        echo "FAIL $name: stdout lacks '$want_out'" >&2
        echo "  got: $out" >&2
        FAILED=$((FAILED + 1))
        return
    fi
    if [ -z "$want_out" ] && [ -n "$out" ]; then
        echo "FAIL $name: expected no stdout, got: $out" >&2
        FAILED=$((FAILED + 1))
        return
    fi
    echo "ok   $name"
}

# Grammars with no superClass keep working unchanged.
check "json tree" '{"a":[1,2]}' 0 '(json (value (obj {' '' \
    -- tests/grammars/JSON.g4 json
check "json parse error" '{"a":' 2 '(json' '' \
    -- tests/grammars/JSON.g4 json
check "json tokens" '{"a":1}' 0 "[@0,0:0='{'" '' \
    -- --tokens tests/grammars/JSON.g4

# A superClass grammar with no base class is refused, not compiled and failed.
check "superClass unsupplied" '0x1f' 1 '' \
    "declares 'options { superClass = RustLexerBase; }'" \
    -- --tokens tests/grammars/RustLexer.g4
check "stub-super without superClass" '{"a":1}' 1 '' 'declares no superClass' \
    -- --tokens --stub-super tests/grammars/JSON.g4
check "super and stub-super conflict" '0x1f' 1 '' 'mutually exclusive' \
    -- --tokens --stub-super --super "$RUST_BASE" tests/grammars/RustLexer.g4

# --super: the real base class, so this is ANTLR4's actual answer. `1.` is the
# case the base class decides — FLOAT_LITERAL only because FloatDotPossible()
# says so.
check "super 0x1f" '0x1f' 0 "'0x1f',<INTEGER_LITERAL>" '' \
    -- --tokens --super "$RUST_BASE" tests/grammars/RustLexer.g4
check "super float dot" '1.' 0 "'1.',<FLOAT_LITERAL>" '' \
    -- --tokens --super "$RUST_BASE" tests/grammars/RustLexer.g4

# --stub-super: answers only where the base class provably cannot matter.
check "stub agrees on 0x1f" '0x1f' 0 "'0x1f',<INTEGER_LITERAL>" 'does not depend on RustLexerBase' \
    -- --tokens --stub-super tests/grammars/RustLexer.g4
check "stub refuses float dot" '1.' 3 '' 'depends on a RustLexerBase predicate' \
    -- --tokens --stub-super tests/grammars/RustLexer.g4
check "stub agrees on typescript" 'let x = 1;' 0 "'let'" '' \
    -- --tokens --stub-super tests/grammars/TypeScriptLexer.g4
check "stub refuses typescript brace" 'function f() {}' 3 '' 'ProcessOpenBrace' \
    -- --tokens --stub-super tests/grammars/TypeScriptLexer.g4
check "stub agrees on antlr4 rule" 'A : B ;' 0 "'A',<ID>" '' \
    -- --tokens --stub-super tests/grammars/ANTLRv4Lexer.g4
check "stub refuses antlr4 argument" 'a[int x] : B ;' 3 '' 'handleBeginArgument' \
    -- --tokens --stub-super tests/grammars/ANTLRv4Lexer.g4

if [ "$FAILED" -ne 0 ]; then
    echo "$FAILED check(s) failed" >&2
    exit 1
fi
echo "all checks passed"
