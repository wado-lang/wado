import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import * as vscode from 'vscode';

/**
 * Create a unique temp directory for a test.
 */
export function createTmpDir(prefix: string): string {
    return fs.mkdtempSync(path.join(os.tmpdir(), prefix));
}

/**
 * Tear down a temp directory created for a test.
 *
 * On Windows, VS Code's file watcher and the LSP client may still hold
 * handles to files under `tmpDir` for a brief moment after the test body
 * resolves, which makes a naive `rmSync` race with `EBUSY` / `EPERM`.
 * Closing the editors first releases VS Code's references, and the retry
 * options on `rmSync` cover the remaining window where the LSP server is
 * still flushing.
 */
export async function cleanupTmpDir(tmpDir: string): Promise<void> {
    try {
        await vscode.commands.executeCommand('workbench.action.closeAllEditors');
    } catch {
        /* ignore — best-effort cleanup */
    }
    fs.rmSync(tmpDir, {
        recursive: true,
        force: true,
        maxRetries: 10,
        retryDelay: 100,
    });
}
