import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';

const HELPER_SOURCE = `pub fn forty_two() -> i32 {
    return 42;
}
`;

const IMPORTER_SOURCE = `use { forty_two } from "./helper.wado";

fn main() -> i32 {
    let x: i32 = forty_two();
    let y: i32 = "bad";
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

async function waitForDiagnostics(uri: vscode.Uri, timeoutMs: number): Promise<vscode.Diagnostic[]> {
    const existing = vscode.languages.getDiagnostics(uri);
    if (existing.length > 0) {
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
            if (diags.length > 0) {
                clearTimeout(timer);
                subscription.dispose();
                resolve(diags);
            }
        });
    });
}

suite('Wado LSP (workspace import)', () => {
    let helperPath: string;
    let importerPath: string;

    suiteSetup(async function () {
        if (!wasmArtifactExists()) {
            this.skip();
        }
        const folders = vscode.workspace.workspaceFolders;
        if (!folders || folders.length === 0) {
            this.skip();
        }
        const root = folders![0].uri.fsPath;
        helperPath = path.join(root, 'helper.wado');
        importerPath = path.join(root, 'importer.wado');
        fs.writeFileSync(helperPath, HELPER_SOURCE, 'utf8');
        fs.writeFileSync(importerPath, IMPORTER_SOURCE, 'utf8');
        const extension = vscode.extensions.getExtension('wado-lang.wado');
        assert.ok(extension, 'Extension should be present');
        if (!extension.isActive) {
            await extension.activate();
        }
    });

    suiteTeardown(() => {
        for (const p of [helperPath, importerPath]) {
            try {
                fs.unlinkSync(p);
            } catch {
                /* ignore */
            }
        }
    });

    test('reports diagnostics for a file that imports a sibling module', async function () {
        this.timeout(120_000);

        const uri = vscode.Uri.file(importerPath);
        const doc = await vscode.workspace.openTextDocument(uri);
        await vscode.window.showTextDocument(doc);

        const diagnostics = await waitForDiagnostics(uri, 90_000);
        const errors = diagnostics.filter((d) => d.severity === vscode.DiagnosticSeverity.Error);
        assert.ok(
            errors.length > 0,
            `Expected at least one error for the bad String-to-i32 assignment, got: ${JSON.stringify(
                diagnostics.map((d) => ({ severity: d.severity, message: d.message })),
            )}`,
        );
        // The error should concern the `let y: i32 = "bad"` statement, not the
        // resolved `forty_two()` call. A diagnostic that complains about
        // `forty_two` would indicate the import failed to resolve across the
        // workspaceFolder mount.
        const importErrors = errors.filter((d) => /forty_two|helper/i.test(d.message));
        assert.strictEqual(
            importErrors.length,
            0,
            `Import resolution failed across workspaceFolder mount: ${JSON.stringify(
                importErrors.map((d) => d.message),
            )}`,
        );
    });
});
