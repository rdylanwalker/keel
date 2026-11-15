import js from '@eslint/js';
import tseslint from 'typescript-eslint';
import prettier from 'eslint-config-prettier';

export default tseslint.config(js.configs.recommended, ...tseslint.configs.recommended, prettier, {
  ignores: ['node_modules/', 'playwright-report/', 'test-results/'],
  languageOptions: {
    globals: {
      console: 'readonly',
      process: 'readonly',
      setTimeout: 'readonly',
      URL: 'readonly',
    },
  },
});
