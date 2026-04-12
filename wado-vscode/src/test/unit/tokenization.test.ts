import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';
import * as vsctm from 'vscode-textmate';
import * as oniguruma from 'vscode-oniguruma';

// Tokenization tests using vscode-textmate
// These tests verify that the grammar correctly tokenizes Wado code

describe('Tokenization Tests', () => {
    let registry: vsctm.Registry;
    let grammar: vsctm.IGrammar | null;

    before(async function () {
        this.timeout(10000); // Loading oniguruma wasm can take time

        // Load oniguruma WASM
        const wasmPath = path.join(
            path.dirname(require.resolve('vscode-oniguruma')),
            'onig.wasm'
        );
        const wasmBin = fs.readFileSync(wasmPath).buffer;
        await oniguruma.loadWASM(wasmBin);

        // Create registry with our grammar
        registry = new vsctm.Registry({
            onigLib: Promise.resolve({
                createOnigScanner: (patterns: string[]) => new oniguruma.OnigScanner(patterns),
                createOnigString: (s: string) => new oniguruma.OnigString(s),
            }),
            loadGrammar: async (scopeName: string) => {
                if (scopeName === 'source.wado') {
                    const grammarPath = path.resolve(__dirname, '../../../syntaxes/wado.tmLanguage.json');
                    const content = fs.readFileSync(grammarPath, 'utf-8');
                    return vsctm.parseRawGrammar(content, grammarPath);
                }
                if (scopeName === 'source.json') {
                    // Return a minimal JSON grammar for testing
                    return vsctm.parseRawGrammar(JSON.stringify({
                        scopeName: 'source.json',
                        patterns: [
                            {
                                name: 'string.quoted.double.json',
                                begin: '"',
                                end: '"',
                                patterns: [
                                    { match: '\\\\.', name: 'constant.character.escape.json' }
                                ]
                            },
                            { match: '\\b(true|false|null)\\b', name: 'constant.language.json' },
                            { match: '-?\\d+(\\.\\d+)?([eE][+-]?\\d+)?', name: 'constant.numeric.json' },
                            { match: '[{}\\[\\],:]', name: 'punctuation.json' }
                        ]
                    }), 'source.json');
                }
                return null;
            }
        });

        grammar = await registry.loadGrammar('source.wado');
    });

    function tokenizeLine(line: string, prevState: vsctm.StateStack | null = null): {
        tokens: vsctm.IToken[];
        ruleStack: vsctm.StateStack;
    } {
        if (!grammar) {
            throw new Error('Grammar not loaded');
        }
        const result = grammar.tokenizeLine(line, prevState);
        return { tokens: result.tokens, ruleStack: result.ruleStack };
    }

    function tokenizeLines(lines: string[]): { tokens: vsctm.IToken[]; scopes: string[] }[] {
        let ruleStack: vsctm.StateStack | null = null;
        const results: { tokens: vsctm.IToken[]; scopes: string[] }[] = [];

        for (const line of lines) {
            const { tokens, ruleStack: newStack } = tokenizeLine(line, ruleStack);
            ruleStack = newStack;
            results.push({
                tokens,
                scopes: tokens.map(t => t.scopes.join(' '))
            });
        }
        return results;
    }

    function getScopes(line: string, prevState: vsctm.StateStack | null = null): string[][] {
        const { tokens } = tokenizeLine(line, prevState);
        return tokens.map(t => t.scopes);
    }

    describe('__DATA__ section', () => {
        it('should tokenize __DATA__ keyword', () => {
            const scopes = getScopes('__DATA__');
            assert.ok(
                scopes.some(s => s.some(scope => scope.includes('keyword.control.data-section'))),
                `Expected keyword.control.data-section, got: ${JSON.stringify(scopes)}`
            );
        });

        it('should tokenize content after __DATA__ as comment when not JSON', () => {
            const lines = ['__DATA__', 'this is plain text', 'more text'];
            const results = tokenizeLines(lines);

            // Line 0: __DATA__ should be keyword
            assert.ok(
                results[0].scopes.some(s => s.includes('keyword.control.data-section')),
                `__DATA__ should be keyword: ${JSON.stringify(results[0])}`
            );

            // Line 1 and 2: should be in comment scope
            assert.ok(
                results[1].scopes.some(s => s.includes('comment.block.data-section.wado')),
                `Plain text should be comment: ${JSON.stringify(results[1])}`
            );
            assert.ok(
                results[2].scopes.some(s => s.includes('comment.block.data-section.wado')),
                `More text should be comment: ${JSON.stringify(results[2])}`
            );
        });

        it('should tokenize JSON object after __DATA__', () => {
            const lines = ['__DATA__', '{"key": "value"}'];
            const results = tokenizeLines(lines);

            // Line 0: __DATA__ should be keyword
            assert.ok(
                results[0].scopes.some(s => s.includes('keyword.control.data-section')),
                `__DATA__ should be keyword: ${JSON.stringify(results[0])}`
            );

            // Line 1: should have JSON-related scopes (embedded block or JSON scopes)
            const line1Scopes = results[1].scopes.join(' ');
            const hasJsonScope = line1Scopes.includes('meta.embedded.block.json') ||
                                 line1Scopes.includes('source.json') ||
                                 line1Scopes.includes('string.quoted');
            assert.ok(
                hasJsonScope,
                `JSON content should have JSON scopes: ${JSON.stringify(results[1])}`
            );
        });

        it('should tokenize JSON array after __DATA__', () => {
            const lines = ['__DATA__', '[1, 2, 3]'];
            const results = tokenizeLines(lines);

            // Line 0: __DATA__ should be keyword
            assert.ok(
                results[0].scopes.some(s => s.includes('keyword.control.data-section')),
                `__DATA__ should be keyword: ${JSON.stringify(results[0])}`
            );

            // Line 1: should have JSON-related scopes
            const line1Scopes = results[1].scopes.join(' ');
            const hasJsonScope = line1Scopes.includes('meta.embedded.block.json') ||
                                 line1Scopes.includes('source.json') ||
                                 line1Scopes.includes('constant.numeric');
            assert.ok(
                hasJsonScope,
                `JSON array should have JSON scopes: ${JSON.stringify(results[1])}`
            );
        });

        it('should tokenize multiline JSON after __DATA__', () => {
            const lines = [
                '__DATA__',
                '{',
                '  "stdout": "hello"',
                '}'
            ];
            const results = tokenizeLines(lines);

            // All lines after __DATA__ should have JSON scopes
            for (let i = 1; i < results.length; i++) {
                const lineScopes = results[i].scopes.join(' ');
                const hasJsonScope = lineScopes.includes('meta.embedded.block.json') ||
                                     lineScopes.includes('source.json') ||
                                     lineScopes.includes('string.quoted') ||
                                     lineScopes.includes('punctuation');
                assert.ok(
                    hasJsonScope,
                    `Line ${i} should have JSON scopes: ${JSON.stringify(results[i])}`
                );
            }
        });
    });

    describe('Basic tokenization', () => {
        it('should tokenize keywords', () => {
            const scopes = getScopes('fn let if else while for');
            assert.ok(
                scopes.some(s => s.some(scope => scope.includes('keyword'))),
                'Should have keyword scopes'
            );
        });

        it('should tokenize comments', () => {
            const scopes = getScopes('// this is a comment');
            assert.ok(
                scopes.some(s => s.some(scope => scope.includes('comment'))),
                'Should have comment scopes'
            );
        });

        it('should tokenize strings', () => {
            const scopes = getScopes('"hello world"');
            assert.ok(
                scopes.some(s => s.some(scope => scope.includes('string'))),
                'Should have string scopes'
            );
        });
    });

    describe('Declaration keywords scope mapping', () => {
        function findToken(line: string, needle: string): vsctm.IToken | undefined {
            const { tokens } = tokenizeLine(line);
            return tokens.find(t => line.slice(t.startIndex, t.endIndex) === needle);
        }

        const storageTypeKeywords = ['fn', 'let', 'global', 'const', 'struct', 'enum', 'variant', 'flags', 'impl', 'trait', 'type'];
        for (const kw of storageTypeKeywords) {
            it(`should dual-scope \`${kw}\` under storage.type AND keyword.control`, () => {
                const tok = findToken(`${kw} foo`, kw);
                assert.ok(
                    tok && tok.scopes.some(s => s.includes('storage.type'))
                        && tok.scopes.some(s => s.includes('keyword.control')),
                    `${kw} should have both storage.type and keyword.control scopes: ${JSON.stringify(tok)}`
                );
            });
        }

        const storageModifierKeywords = ['pub', 'mut', 'async'];
        for (const kw of storageModifierKeywords) {
            it(`should dual-scope \`${kw}\` under storage.modifier AND keyword.control`, () => {
                const tok = findToken(`${kw} foo`, kw);
                assert.ok(
                    tok && tok.scopes.some(s => s.includes('storage.modifier'))
                        && tok.scopes.some(s => s.includes('keyword.control')),
                    `${kw} should have both storage.modifier and keyword.control scopes: ${JSON.stringify(tok)}`
                );
            });
        }

        it('should tokenize `task` as a control keyword', () => {
            const tok = findToken('task return 42;', 'task');
            assert.ok(
                tok && tok.scopes.some(s => s.includes('keyword.control')),
                `task should be under keyword.control: ${JSON.stringify(tok)}`
            );
        });

        it('should tokenize `return` in `task return` as a control keyword', () => {
            const tok = findToken('task return 42;', 'return');
            assert.ok(
                tok && tok.scopes.some(s => s.includes('keyword.control')),
                `return should be under keyword.control: ${JSON.stringify(tok)}`
            );
        });
    });

    describe('Compile-time literals', () => {
        const literals = ['#file', '#line', '#function', '#data'];
        for (const lit of literals) {
            it(`should tokenize ${lit} as compile-time literal`, () => {
                const { tokens } = tokenizeLine(`let x = ${lit};`);
                const hit = tokens.find(t =>
                    t.scopes.some(s => s.includes('constant.language.compile-time'))
                );
                assert.ok(
                    hit,
                    `${lit} should have constant.language.compile-time scope: ${JSON.stringify(tokens)}`
                );
            });
        }

        it('should tokenize #include_str(...) as compile-time literal', () => {
            const { tokens } = tokenizeLine('let x = #include_str("./foo.wado");');
            const hit = tokens.find(t =>
                t.scopes.some(s => s.includes('constant.language.compile-time'))
            );
            assert.ok(
                hit,
                `#include_str should have compile-time scope: ${JSON.stringify(tokens)}`
            );
        });

        it('should tokenize #include_bytes(...) as compile-time literal', () => {
            const { tokens } = tokenizeLine('let x = #include_bytes("./icon.png");');
            const hit = tokens.find(t =>
                t.scopes.some(s => s.includes('constant.language.compile-time'))
            );
            assert.ok(
                hit,
                `#include_bytes should have compile-time scope: ${JSON.stringify(tokens)}`
            );
        });
    });

    describe('Operators from OperatorCategories::other', () => {
        function findToken(line: string, needle: string): vsctm.IToken | undefined {
            const { tokens } = tokenizeLine(line);
            return tokens.find(t => line.slice(t.startIndex, t.endIndex) === needle);
        }

        it('should tokenize `matches` as an operator', () => {
            const tok = findToken('if opt matches { Some(_) } { }', 'matches');
            assert.ok(
                tok && tok.scopes.some(s => s.includes('keyword.operator')),
                `matches should have keyword.operator scope: ${JSON.stringify(tok)}`
            );
        });

        it('should tokenize `..<` as a single range operator', () => {
            const tok = findToken('for let i of 0..<10 { }', '..<');
            assert.ok(
                tok && tok.scopes.some(s => s.includes('keyword.operator')),
                `..< should be one token with keyword.operator scope: ${JSON.stringify(tok)}`
            );
        });

        it('should tokenize `..=` as a single range operator', () => {
            const tok = findToken('for let i of 1..=10 { }', '..=');
            assert.ok(
                tok && tok.scopes.some(s => s.includes('keyword.operator')),
                `..= should be one token with keyword.operator scope: ${JSON.stringify(tok)}`
            );
        });
    });
});
