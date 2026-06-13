import * as assert from 'assert';
import * as fs from 'fs';
import * as path from 'path';
import * as vscode from 'vscode';

const WASM_ARTIFACT_RELATIVE = path.join('out', 'wado_lsp.wasm');

function extensionRoot(): string {
    return path.resolve(__dirname, '..', '..', '..');
}

function wasmArtifactExists(): boolean {
    return fs.existsSync(path.join(extensionRoot(), WASM_ARTIFACT_RELATIVE));
}

/** Find the open tab whose input resolves to `uri`, if any. */
function findTabFor(uri: vscode.Uri): vscode.Tab | undefined {
    for (const group of vscode.window.tabGroups.all) {
        for (const tab of group.tabs) {
            const input = tab.input;
            if (
                input instanceof vscode.TabInputText &&
                input.uri.toString() === uri.toString()
            ) {
                return tab;
            }
        }
    }
    return undefined;
}

async function waitForTab(
    uri: vscode.Uri,
    timeoutMs: number,
): Promise<vscode.Tab> {
    const existing = findTabFor(uri);
    if (existing) {
        return existing;
    }
    return new Promise((resolve, reject) => {
        const timer = setTimeout(() => {
            subscription.dispose();
            reject(new Error(`Timed out after ${timeoutMs}ms waiting for a tab for ${uri.toString()}`));
        }, timeoutMs);
        const subscription = vscode.window.tabGroups.onDidChangeTabs(() => {
            const tab = findTabFor(uri);
            if (tab) {
                clearTimeout(timer);
                subscription.dispose();
                resolve(tab);
            }
        });
    });
}

suite('Wado LSP (stdlib editor label)', () => {
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

    // The bundled stdlib modules are served as virtual `core:` / `wasi:`
    // documents. Without the `resourceLabelFormatters` contribution VS Code
    // would label the tab with the URI basename only (`json`); the formatter
    // renders the full `core:json` so the scheme stays visible.
    test('labels a core: stdlib document with its scheme', async function () {
        this.timeout(120_000);

        const uri = vscode.Uri.parse('core:json');
        const doc = await vscode.workspace.openTextDocument(uri);
        await vscode.window.showTextDocument(doc);

        const tab = await waitForTab(uri, 90_000);
        assert.strictEqual(
            tab.label,
            'core:json',
            `Expected the editor tab label to include the scheme, got: ${tab.label}`,
        );
    });
});
