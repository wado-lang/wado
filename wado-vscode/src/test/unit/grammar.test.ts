import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';

// Unit tests for the TextMate grammar
// These tests verify the grammar file is valid JSON and has required structure

describe('Grammar Unit Tests', () => {
    const grammarPath = path.resolve(__dirname, '../../../syntaxes/wado.tmLanguage.json');
    let grammar: {
        name: string;
        scopeName: string;
        patterns: Array<{ include?: string }>;
        repository: Record<string, unknown>;
    };

    before(() => {
        const content = fs.readFileSync(grammarPath, 'utf-8');
        grammar = JSON.parse(content);
    });

    it('Grammar file should be valid JSON', () => {
        assert.ok(grammar, 'Grammar should be parsed');
    });

    it('Grammar should have correct scopeName', () => {
        assert.strictEqual(grammar.scopeName, 'source.wado');
    });

    it('Grammar should have a name', () => {
        assert.strictEqual(grammar.name, 'Wado');
    });

    it('Grammar should have patterns array', () => {
        assert.ok(Array.isArray(grammar.patterns), 'patterns should be an array');
        assert.ok(grammar.patterns.length > 0, 'patterns should not be empty');
    });

    it('Grammar should have repository', () => {
        assert.ok(grammar.repository, 'repository should exist');
    });

    it('Repository should have comments pattern', () => {
        assert.ok(grammar.repository.comments, 'comments pattern should exist');
    });

    it('Repository should have keywords pattern', () => {
        assert.ok(grammar.repository.keywords, 'keywords pattern should exist');
    });

    it('Repository should have strings pattern', () => {
        assert.ok(grammar.repository.strings, 'strings pattern should exist');
    });

    it('Repository should have numbers pattern', () => {
        assert.ok(grammar.repository.numbers, 'numbers pattern should exist');
    });

    it('Repository should have types pattern', () => {
        assert.ok(grammar.repository.types, 'types pattern should exist');
    });

    it('All pattern includes should reference existing repository items', () => {
        const repoKeys = new Set(Object.keys(grammar.repository));

        function checkIncludes(patterns: Array<{ include?: string; patterns?: Array<{ include?: string }> }>) {
            for (const pattern of patterns) {
                if (pattern.include && pattern.include.startsWith('#')) {
                    const ref = pattern.include.slice(1);
                    assert.ok(repoKeys.has(ref), `Include reference #${ref} should exist in repository`);
                }
                if (pattern.patterns) {
                    checkIncludes(pattern.patterns);
                }
            }
        }

        checkIncludes(grammar.patterns);
    });
});
