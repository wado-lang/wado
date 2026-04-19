import { defineConfig } from '@vscode/test-cli';
import * as path from 'path';
import { fileURLToPath } from 'url';

const here = path.dirname(fileURLToPath(import.meta.url));
const fixtureWorkspace = path.join(here, 'src', 'test', 'fixtures', 'workspace');

export default defineConfig({
  files: 'out/test/suite/**/*.test.js',
  workspaceFolder: fixtureWorkspace,
  mocha: {
    ui: 'tdd',
    timeout: 20000,
  },
});
