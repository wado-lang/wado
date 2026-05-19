import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';
import { cleanupTmpDir, createTmpDir } from './tmp-workspace';

const BROKEN_SOURCE = `fn main() -> i32 {
    let x: i32 = "not an integer";
    return x;
}
`;

const WASM_ARTIFACT_RELATIVE = path.join('out', 'wado_lsp.wasm');

function extensionRoot(): string {
    return path.resolve(__dirname, '..', '..', '..');
}

function wasmArtifactExists(): boolean {
    return fs.existsSync(path.join(extensionRoot(), WASM_ARTIFACT_RELATIVE));
}

async function waitForError(uri: vscode.Uri, timeoutMs: number): Promise<vscode.Diagnostic[]> {
    const hasError = (ds: vscode.Diagnostic[]) =>
        ds.some((d) => d.severity === vscode.DiagnosticSeverity.Error);
    const existing = vscode.languages.getDiagnostics(uri);
    if (hasError(existing)) {
        return existing;
    }
    return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
            subscription.dispose();
            reject(new Error(`Timed out after ${timeoutMs}ms waiting for diagnostics on ${uri.toString()}`));
        }, timeoutMs);
        const subscription = vscode.languages.onDidChangeDiagnostics((event) => {
            if (!event.uris.some((u) => u.toString() === uri.toString())) {
                return;
            }
            const diags = vscode.languages.getDiagnostics(uri);
            if (hasError(diags)) {
                clearTimeout(timer);
                subscription.dispose();
                resolve(diags);
            }
        });
    });
}

suite('Wado LSP (restart command)', () => {
    suiteSetup(async function () {
        if (!wasmArtifactExists()) {
            this.skip();
        }
        const extension = vscode.extensions.getExtension('wado-lang.wado');
        assert.ok(extension, 'Extension should be present');
        if (!extension.isActive) {
            await extension.activate();
        }
    });

    test('restart command re-publishes diagnostics from a fresh client', async function () {
        this.timeout(90_000);

        const tmpDir = createTmpDir('wado-lsp-restart-');
        const filePath = path.join(tmpDir, 'broken.wado');
        fs.writeFileSync(filePath, BROKEN_SOURCE, 'utf8');

        try {
            const uri = vscode.Uri.file(filePath);
            const doc = await vscode.workspace.openTextDocument(uri);
            await vscode.window.showTextDocument(doc);

            const initial = await waitForError(uri, 60_000);
            assert.ok(
                initial.some((d) => d.severity === vscode.DiagnosticSeverity.Error),
                'Initial diagnostics should include an error',
            );

            // Listen *before* kicking the restart — otherwise the re-publish may
            // arrive while the await below is still settling.
            const republished = new Promise<vscode.Diagnostic[]>((resolve, reject) => {
                const timer = setTimeout(() => {
                    subscription.dispose();
                    reject(new Error('Timed out waiting for post-restart diagnostics'));
                }, 30_000);
                let sawClear = false;
                const subscription = vscode.languages.onDidChangeDiagnostics((event) => {
                    if (!event.uris.some((u) => u.toString() === uri.toString())) {
                        return;
                    }
                    const diags = vscode.languages.getDiagnostics(uri);
                    const hasError = diags.some(
                        (d) => d.severity === vscode.DiagnosticSeverity.Error,
                    );
                    // Require a clear-then-republish sequence to prove the fresh
                    // client actually emitted, not just the stale collection.
                    if (!hasError) {
                        sawClear = sawClear || diags.length === 0;
                        return;
                    }
                    if (sawClear) {
                        clearTimeout(timer);
                        subscription.dispose();
                        resolve(diags);
                    }
                });
            });

            await vscode.commands.executeCommand('wado.restartLanguageServer');

            const after = await republished;
            assert.ok(
                after.some((d) => d.severity === vscode.DiagnosticSeverity.Error),
                'Post-restart diagnostics should include an error',
            );
        } finally {
            await cleanupTmpDir(tmpDir);
        }
    });
});
