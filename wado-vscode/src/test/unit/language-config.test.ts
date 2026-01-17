import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';

// Unit tests for the language configuration

describe('Language Configuration Unit Tests', () => {
    const configPath = path.resolve(__dirname, '../../../language-configuration.json');
    let config: {
        comments: { lineComment: string; blockComment: [string, string] };
        brackets: Array<[string, string]>;
        autoClosingPairs: Array<{ open: string; close: string; notIn?: string[] }>;
        surroundingPairs: Array<[string, string]>;
    };

    before(() => {
        const content = fs.readFileSync(configPath, 'utf-8');
        config = JSON.parse(content);
    });

    it('Language config should be valid JSON', () => {
        assert.ok(config, 'Config should be parsed');
    });

    it('Should have line comment configured', () => {
        assert.strictEqual(config.comments.lineComment, '//');
    });

    it('Should have block comment configured', () => {
        assert.deepStrictEqual(config.comments.blockComment, ['/*', '*/']);
    });

    it('Should have brackets configured', () => {
        const expectedBrackets = [
            ['{', '}'],
            ['[', ']'],
            ['(', ')'],
            ['<', '>'],
        ];
        for (const bracket of expectedBrackets) {
            assert.ok(
                config.brackets.some(b => b[0] === bracket[0] && b[1] === bracket[1]),
                `Bracket pair ${bracket[0]}${bracket[1]} should be configured`
            );
        }
    });

    it('Should have auto-closing pairs for common delimiters', () => {
        const expectedPairs = ['{', '[', '(', '"', "'", '`'];
        for (const open of expectedPairs) {
            assert.ok(
                config.autoClosingPairs.some(p => p.open === open),
                `Auto-closing pair for ${open} should be configured`
            );
        }
    });

    it('Should have surrounding pairs', () => {
        assert.ok(Array.isArray(config.surroundingPairs), 'surroundingPairs should be an array');
        assert.ok(config.surroundingPairs.length > 0, 'surroundingPairs should not be empty');
    });
});
