import * as vscode from 'vscode';
import { LanguageClient, LanguageClientOptions, ServerOptions, StreamInfo } from 'vscode-languageclient/node';
import { Wasm } from '@vscode/wasm-wasi/v1';
import { createStdioOptions, createUriConverters, startServer } from '@vscode/wasm-wasi-lsp';
import * as cp from 'child_process';

const LANGUAGE_ID = 'wado';
const CLIENT_ID = 'wadoLanguageServer';
const CLIENT_NAME = 'Wado Language Server';
const WASM_PATH_SEGMENTS = ['out', 'wado_lsp.wasm'] as const;
const RESTART_DEBOUNCE_MS = 250;

let outputChannel: vscode.LogOutputChannel | undefined;
let client: LanguageClient | undefined;
let lifecycleQueue: Promise<void> = Promise.resolve();
let pendingRestart: NodeJS.Timeout | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<void> {
    outputChannel = vscode.window.createOutputChannel(CLIENT_NAME, { log: true });
    context.subscriptions.push(outputChannel);

    context.subscriptions.push(
        vscode.commands.registerCommand('wado.restartLanguageServer', () =>
            enqueueLifecycle(async () => {
                await stopClientLocked();
                await startClientLocked(context);
            }),
        ),
    );

    const wasmWatcher = vscode.workspace.createFileSystemWatcher(
        new vscode.RelativePattern(context.extensionUri, WASM_PATH_SEGMENTS.join('/')),
    );
    const scheduleRestartOnWasmChange = () => {
        if (usingNativeOverride()) {
            return;
        }
        if (pendingRestart) {
            clearTimeout(pendingRestart);
        }
        pendingRestart = setTimeout(() => {
            pendingRestart = undefined;
            outputChannel?.info('Bundled wado_lsp.wasm changed — restarting server.');
            enqueueLifecycle(async () => {
                await stopClientLocked();
                await startClientLocked(context);
            }).catch((err: unknown) => {
                outputChannel?.error(`Auto-restart failed: ${String(err)}`);
            });
        }, RESTART_DEBOUNCE_MS);
    };
    wasmWatcher.onDidChange(scheduleRestartOnWasmChange);
    wasmWatcher.onDidCreate(scheduleRestartOnWasmChange);
    context.subscriptions.push(wasmWatcher);

    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration((event) => {
            if (!event.affectsConfiguration('wado.serverPath')) {
                return;
            }
            outputChannel?.info('wado.serverPath changed — restarting server.');
            enqueueLifecycle(async () => {
                await stopClientLocked();
                await startClientLocked(context);
            }).catch((err: unknown) => {
                outputChannel?.error(`Config-change restart failed: ${String(err)}`);
            });
        }),
    );

    context.subscriptions.push(
        vscode.workspace.onDidChangeWorkspaceFolders(() => {
            outputChannel?.info('Workspace folders changed — restarting server.');
            enqueueLifecycle(async () => {
                await stopClientLocked();
                await startClientLocked(context);
            }).catch((err: unknown) => {
                outputChannel?.error(`Workspace-change restart failed: ${String(err)}`);
            });
        }),
    );

    await enqueueLifecycle(() => startClientLocked(context));
}

export async function deactivate(): Promise<void> {
    if (pendingRestart) {
        clearTimeout(pendingRestart);
        pendingRestart = undefined;
    }
    await enqueueLifecycle(stopClientLocked);
}

function enqueueLifecycle(op: () => Promise<void>): Promise<void> {
    const next = lifecycleQueue.then(op, op);
    lifecycleQueue = next.catch(() => {
        /* swallow so a failure doesn't poison the queue */
    });
    return next;
}

async function startClientLocked(context: vscode.ExtensionContext): Promise<void> {
    if (client) {
        return;
    }
    let serverOptions: ServerOptions;
    try {
        serverOptions = buildServerOptions(context);
    } catch (err) {
        outputChannel?.error(`Failed to build server options: ${String(err)}`);
        vscode.window.showErrorMessage(
            `${CLIENT_NAME} could not initialize. See the "${CLIENT_NAME}" output channel for details.`,
        );
        return;
    }
    const clientOptions: LanguageClientOptions = {
        documentSelector: [{ language: LANGUAGE_ID }],
        outputChannel,
        diagnosticCollectionName: LANGUAGE_ID,
        uriConverters: createUriConverters(),
    };
    const pending = new LanguageClient(CLIENT_ID, CLIENT_NAME, serverOptions, clientOptions);
    try {
        await pending.start();
        client = pending;
    } catch (err) {
        outputChannel?.error(`Failed to start ${CLIENT_NAME}: ${String(err)}`);
        try {
            await pending.dispose();
        } catch (disposeErr) {
            outputChannel?.error(`Failed to dispose half-started client: ${String(disposeErr)}`);
        }
        vscode.window.showWarningMessage(
            `${CLIENT_NAME} failed to start. Run "Wado: Restart Language Server" after resolving the issue. See the "${CLIENT_NAME}" output channel for details.`,
        );
    }
}

async function stopClientLocked(): Promise<void> {
    if (!client) {
        return;
    }
    const running = client;
    client = undefined;
    try {
        await running.stop();
    } catch (err) {
        outputChannel?.error(`Error stopping ${CLIENT_NAME}: ${String(err)}`);
    }
}

function usingNativeOverride(): boolean {
    return Boolean(vscode.workspace.getConfiguration('wado').get<string>('serverPath')?.trim());
}

function buildServerOptions(context: vscode.ExtensionContext): ServerOptions {
    const configuredPath = vscode.workspace.getConfiguration('wado').get<string>('serverPath')?.trim();
    if (configuredPath) {
        return buildSubprocessServerOptions(configuredPath);
    }
    return buildWasmServerOptions(context);
}

function buildSubprocessServerOptions(serverPath: string): ServerOptions {
    outputChannel?.info(`Using native Wado LSP binary: ${serverPath}`);
    return () => {
        const child = cp.spawn(serverPath, [], { stdio: ['pipe', 'pipe', 'pipe'] });
        child.on('error', (err) => {
            outputChannel?.error(`Failed to spawn ${serverPath}: ${String(err)}`);
        });
        child.stderr?.on('data', (chunk: Buffer) => {
            outputChannel?.append(chunk.toString('utf8'));
        });
        if (!child.stdin || !child.stdout) {
            return Promise.reject(new Error(`Native server ${serverPath} did not expose stdio streams.`));
        }
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
        const wasmUri = vscode.Uri.joinPath(context.extensionUri, ...WASM_PATH_SEGMENTS);
        const bytes = await vscode.workspace.fs.readFile(wasmUri);
        const module = await WebAssembly.compile(toArrayBuffer(bytes));
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

// `vscode.workspace.fs.readFile` returns `Uint8Array<ArrayBufferLike>` because
// the backing buffer could theoretically be a `SharedArrayBuffer`. `WebAssembly.compile`
// wants a plain `ArrayBuffer`. Slicing into a fresh `ArrayBuffer` is guaranteed
// to produce a contiguous, non-shared buffer regardless of the source backing.
function toArrayBuffer(view: Uint8Array): ArrayBuffer {
    const copy = new ArrayBuffer(view.byteLength);
    new Uint8Array(copy).set(view);
    return copy;
}
