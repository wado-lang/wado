import { defineConfig } from '@vscode/test-cli';
import * as path from 'path';
import { fileURLToPath } from 'url';

const here = path.dirname(fileURLToPath(import.meta.url));
const fixtureWorkspace = path.join(here, 'src', 'test', 'fixtures', 'workspace');

export default defineConfig({
  // Pinned so a VS Code release cannot break CI on its own.
  version: '1.131.0',
  files: 'out/test/suite/**/*.test.js',
  workspaceFolder: fixtureWorkspace,
  mocha: {
    ui: 'tdd',
    timeout: 20000,
  },
});
