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

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/**
 * Open a stdlib virtual document, retrying while the language client is still
 * starting up. The `core:`/`wasi:` content provider throws "Wado language
 * server is not running" until the client is ready, and across the full test
 * run our suite may execute before the client has finished launching.
 */
async function openStdlibDocument(
    uri: vscode.Uri,
    timeoutMs: number,
): Promise<vscode.TextDocument> {
    const deadline = Date.now() + timeoutMs;
    let lastError: unknown;
    for (;;) {
        try {
            return await vscode.workspace.openTextDocument(uri);
        } catch (err) {
            lastError = err;
            if (Date.now() >= deadline) {
                break;
            }
            await sleep(500);
        }
    }
    throw new Error(
        `Timed out after ${timeoutMs}ms opening ${uri.toString()}: ${String(lastError)}`,
    );
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

    // `resourceLabelFormatters` renders the tab as `core:json`, not `json`.
    test('labels a core: stdlib document with its scheme', async function () {
        this.timeout(120_000);

        const uri = vscode.Uri.parse('core:json');
        const doc = await openStdlibDocument(uri, 90_000);
        await vscode.window.showTextDocument(doc);

        const tab = await waitForTab(uri, 90_000);
        assert.strictEqual(
            tab.label,
            'core:json',
            `Expected the editor tab label to include the scheme, got: ${tab.label}`,
        );
    });

    // Submodules with a `/` keep the basename: VS Code basenames the label.
    test('labels a core: submodule document with its basename', async function () {
        this.timeout(120_000);

        const uri = vscode.Uri.parse('core:prelude/types.wado');
        const doc = await openStdlibDocument(uri, 90_000);
        await vscode.window.showTextDocument(doc);

        const tab = await waitForTab(uri, 90_000);
        assert.strictEqual(
            tab.label,
            'types.wado',
            `Expected the submodule tab label to be the basename, got: ${tab.label}`,
        );
    });
});
