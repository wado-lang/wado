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
 *
 * Mitigations, in order:
 *   1. `closeAllEditors` releases VS Code's references.
 *   2. On Windows, a short async tick gives the file watcher a chance to
 *      unsubscribe before we start hammering the directory. Other
 *      platforms allow unlinking open files, so we skip the wait there.
 *   3. `rmSync` retries for ~6 seconds to ride out the LSP server's
 *      shutdown flush.
 *   4. If the directory still refuses to die *with a known Windows lock
 *      error*, log and move on — the test assertion has already run, and
 *      CI runners are ephemeral so the OS will reap the leftover. Any
 *      other error (bad path, EACCES, …) is rethrown so we don't mask
 *      real harness bugs.
 */
const WINDOWS_LOCK_ERRNOS = new Set(['EBUSY', 'EPERM', 'ENOTEMPTY']);

export async function cleanupTmpDir(tmpDir: string): Promise<void> {
    try {
        await vscode.commands.executeCommand('workbench.action.closeAllEditors');
    } catch {
        /* ignore — best-effort cleanup */
    }
    if (process.platform === 'win32') {
        await new Promise((r) => setTimeout(r, 200));
    }
    try {
        fs.rmSync(tmpDir, {
            recursive: true,
            force: true,
            maxRetries: 30,
            retryDelay: 200,
        });
    } catch (err) {
        const code = (err as NodeJS.ErrnoException).code;
        if (process.platform === 'win32' && code !== undefined && WINDOWS_LOCK_ERRNOS.has(code)) {
            console.warn(`cleanupTmpDir: leaving ${tmpDir} behind (${code})`);
            return;
        }
        throw err;
    }
}
