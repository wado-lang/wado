import * as vscode from 'vscode';
import { LanguageClient, LanguageClientOptions, ServerOptions, StreamInfo } from 'vscode-languageclient/node';
import { Wasm } from '@vscode/wasm-wasi/v1';
import { createStdioOptions, createUriConverters, startServer } from '@vscode/wasm-wasi-lsp';
import * as cp from 'child_process';

const LANGUAGE_ID = 'wado';
const CLIENT_ID = 'wadoLanguageServer';
const CLIENT_NAME = 'Wado Language Server';
const WASM_RELATIVE_PATH = 'out/wado_lsp.wasm';

let client: LanguageClient | undefined;
let wasmWatcher: vscode.FileSystemWatcher | undefined;
let outputChannel: vscode.LogOutputChannel | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
    outputChannel = vscode.window.createOutputChannel(CLIENT_NAME, { log: true });
    context.subscriptions.push(outputChannel);

    context.subscriptions.push(
        vscode.commands.registerCommand('wado.version', () => {
            const version = context.extension.packageJSON.version as string;
            vscode.window.showInformationMessage(`Wado Language Support v${version}`);
        }),
    );

    context.subscriptions.push(
        vscode.commands.registerCommand('wado.restartLanguageServer', async () => {
            await restartClient(context);
        }),
    );

    wasmWatcher = vscode.workspace.createFileSystemWatcher(
        new vscode.RelativePattern(context.extensionUri, WASM_RELATIVE_PATH),
    );
    const onWasmChanged = async () => {
        if (!vscode.workspace.getConfiguration('wado').get<string>('serverPath')) {
            outputChannel?.appendLine('Bundled wado_lsp.wasm changed — restarting server.');
            await restartClient(context);
        }
    };
    wasmWatcher.onDidChange(onWasmChanged);
    wasmWatcher.onDidCreate(onWasmChanged);
    context.subscriptions.push(wasmWatcher);

    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration(async (event) => {
            if (event.affectsConfiguration('wado.serverPath')) {
                outputChannel?.appendLine('wado.serverPath changed — restarting server.');
                await restartClient(context);
            }
        }),
    );

    await startClient(context);
}

export async function deactivate(): Promise<void> {
    await stopClient();
}

async function startClient(context: vscode.ExtensionContext): Promise<void> {
    if (client) {
        return;
    }
    const serverOptions = await buildServerOptions(context);
    const clientOptions: LanguageClientOptions = {
        documentSelector: [{ language: LANGUAGE_ID }],
        outputChannel,
        diagnosticCollectionName: LANGUAGE_ID,
        uriConverters: createUriConverters(),
    };
    client = new LanguageClient(CLIENT_ID, CLIENT_NAME, serverOptions, clientOptions);
    try {
        await client.start();
    } catch (err) {
        outputChannel?.appendLine(`Failed to start ${CLIENT_NAME}: ${String(err)}`);
        client = undefined;
        throw err;
    }
}

async function stopClient(): Promise<void> {
    if (!client) {
        return;
    }
    const running = client;
    client = undefined;
    try {
        await running.stop();
    } catch (err) {
        outputChannel?.appendLine(`Error stopping ${CLIENT_NAME}: ${String(err)}`);
    }
}

async function restartClient(context: vscode.ExtensionContext): Promise<void> {
    await stopClient();
    await startClient(context);
}

async function buildServerOptions(context: vscode.ExtensionContext): Promise<ServerOptions> {
    const configuredPath = vscode.workspace.getConfiguration('wado').get<string>('serverPath')?.trim();
    if (configuredPath) {
        return buildSubprocessServerOptions(configuredPath);
    }
    return buildWasmServerOptions(context);
}

function buildSubprocessServerOptions(serverPath: string): ServerOptions {
    outputChannel?.appendLine(`Using native Wado LSP binary: ${serverPath}`);
    return () => {
        const child = cp.spawn(serverPath, [], {
            stdio: ['pipe', 'pipe', 'pipe'],
        });
        child.on('error', (err) => {
            outputChannel?.appendLine(`Failed to spawn ${serverPath}: ${String(err)}`);
        });
        child.stderr?.on('data', (chunk: Buffer) => {
            outputChannel?.append(chunk.toString('utf8'));
        });
        const info: StreamInfo = {
            reader: child.stdout,
            writer: child.stdin,
            detached: false,
        };
        return Promise.resolve(info);
    };
}

function buildWasmServerOptions(context: vscode.ExtensionContext): ServerOptions {
    return async () => {
        const wasm = await Wasm.load();
        const wasmUri = vscode.Uri.joinPath(context.extensionUri, ...WASM_RELATIVE_PATH.split('/'));
        const bits = await vscode.workspace.fs.readFile(wasmUri);
        const module = await WebAssembly.compile(bits as unknown as BufferSource);
        const process = await wasm.createProcess('wado-lsp', module, {
            stdio: createStdioOptions(),
            mountPoints: [{ kind: 'workspaceFolder' }],
        });
        const decoder = new TextDecoder('utf-8');
        process.stderr?.onData((data) => {
            outputChannel?.append(decoder.decode(data));
        });
        return startServer(process);
    };
}
