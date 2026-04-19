import * as assert from 'assert';
import * as vscode from 'vscode';
import * as path from 'path';
import * as fs from 'fs';
import * as os from 'os';

suite('Extension Test Suite', () => {
    vscode.window.showInformationMessage('Start all tests.');

    test('Extension should be present', () => {
        const extension = vscode.extensions.getExtension('wado-lang.wado');
        assert.ok(extension, 'Extension should be installed');
    });

    test('Wado language should be registered', async () => {
        const languages = await vscode.languages.getLanguages();
        assert.ok(languages.includes('wado'), 'Wado language should be registered');
    });

    test('wado.restartLanguageServer command should be registered', async () => {
        const extension = vscode.extensions.getExtension('wado-lang.wado');
        if (extension && !extension.isActive) {
            await extension.activate();
        }

        const commands = await vscode.commands.getCommands(true);
        assert.ok(
            commands.includes('wado.restartLanguageServer'),
            'wado.restartLanguageServer command should be registered',
        );
    });

    test('.wado file extension should be associated with wado language', async () => {
        // Create a temporary .wado file
        const tmpDir = os.tmpdir();
        const tmpFile = path.join(tmpDir, `test-${Date.now()}.wado`);
        fs.writeFileSync(tmpFile, 'fn run() { }');

        try {
            // Open the file by URI (not specifying language)
            const uri = vscode.Uri.file(tmpFile);
            const doc = await vscode.workspace.openTextDocument(uri);

            // VS Code should automatically detect it as wado
            assert.strictEqual(doc.languageId, 'wado',
                `Expected languageId 'wado' but got '${doc.languageId}'`);
        } finally {
            // Cleanup
            fs.unlinkSync(tmpFile);
        }
    });
});
