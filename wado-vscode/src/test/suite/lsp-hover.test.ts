import * as assert from 'assert';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as vscode from 'vscode';

const SOURCE = `fn answer() -> i32 {
    return 42;
}

fn main() -> i32 {
    return answer();
}
`;

const WASM_ARTIFACT_RELATIVE = path.join('out', 'wado_lsp.wasm');

function extensionRoot(): string {
    return path.resolve(__dirname, '..', '..', '..');
}

function wasmArtifactExists(): boolean {
    return fs.existsSync(path.join(extensionRoot(), WASM_ARTIFACT_RELATIVE));
}

function renderHover(hovers: vscode.Hover[]): string {
    return hovers
        .flatMap((h) =>
            h.contents.map((c) => {
                if (typeof c === 'string') return c;
                if (c instanceof vscode.MarkdownString) return c.value;
                return typeof c.value === 'string' ? c.value : '';
            }),
        )
        .join('\n');
}

suite('Wado LSP (hover)', () => {
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

    test('returns hover information for a function reference', async function () {
        this.timeout(60_000);

        const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), 'wado-lsp-hover-'));
        const filePath = path.join(tmpDir, 'hover.wado');
        fs.writeFileSync(filePath, SOURCE, 'utf8');

        try {
            const uri = vscode.Uri.file(filePath);
            const doc = await vscode.workspace.openTextDocument(uri);
            await vscode.window.showTextDocument(doc);

            // Position on `answer` inside `return answer();` at line 5.
            const position = new vscode.Position(5, 13);

            // Poll until the LSP client has initialized and returns a non-empty
            // hover — without polling we race the server bootstrap.
            const deadline = Date.now() + 30_000;
            let rendered = '';
            while (Date.now() < deadline) {
                const hovers =
                    (await vscode.commands.executeCommand<vscode.Hover[]>(
                        'vscode.executeHoverProvider',
                        uri,
                        position,
                    )) ?? [];
                rendered = renderHover(hovers);
                if (rendered.trim().length > 0) {
                    break;
                }
                await new Promise((r) => setTimeout(r, 250));
            }

            assert.ok(
                rendered.trim().length > 0,
                'Expected a non-empty hover for `answer`',
            );
            assert.ok(
                /answer|i32/.test(rendered),
                `Hover should reference the symbol or its signature, got: ${rendered}`,
            );
        } finally {
            fs.rmSync(tmpDir, { recursive: true, force: true });
        }
    });
});
