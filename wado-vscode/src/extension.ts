import * as vscode from 'vscode';

// This extension provides language support for Wado.
// Currently it only provides syntax highlighting via TextMate grammar.
// Future versions will integrate with the Wado compiler for LSP support.

export function activate(context: vscode.ExtensionContext): void {
    console.log('Wado language extension activated');

    // Register a simple command for testing
    const disposable = vscode.commands.registerCommand('wado.version', () => {
        vscode.window.showInformationMessage('Wado Language Support v0.0.1');
    });

    context.subscriptions.push(disposable);

    // TODO: Future LSP integration
    // When the Wado compiler is compiled to Wasm, we can:
    // 1. Bundle the Wasm module with the extension
    // 2. Run it as an LSP server
    // 3. Provide features like:
    //    - Diagnostics (errors/warnings)
    //    - Go to definition
    //    - Find references
    //    - Hover information
    //    - Code completion
}

export function deactivate(): void {
    console.log('Wado language extension deactivated');
}
