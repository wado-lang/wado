import { defineConfig } from '@vscode/test-cli';

export default defineConfig({
  files: 'out/test/suite/**/*.test.js',
  mocha: {
    ui: 'tdd',
    timeout: 20000,
  },
});
