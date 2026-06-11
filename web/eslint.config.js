// @ts-check
// Multiview management SPA — ESLint flat config (ESLint 9 + typescript-eslint 8).
// Enforces the "absolute typing" pillar of docs/development/agent-guardrails.md:
//   bans `any` (no-explicit-any + no-unsafe-*), @ts-ignore/@ts-nocheck, and non-null `!`.
// strictTypeChecked is NOT semver-stable: pin typescript-eslint and review the
// changelog before upgrading. Type-aware rules silently disable for any file not
// covered by a tsconfig — keep `projectService` pointing at every linted file.
import eslint from '@eslint/js';
import tseslint from 'typescript-eslint';
import reactHooks from 'eslint-plugin-react-hooks';
import reactRefresh from 'eslint-plugin-react-refresh';
import jsxA11y from 'eslint-plugin-jsx-a11y';

export default tseslint.config(
  // Never lint build output, generated clients, or vendored code.
  // `src/api/schema.ts` is emitted verbatim by openapi-typescript from the
  // OpenAPI spec — it is generated, not hand-authored, so it is not linted.
  {
    ignores: [
      'dist/**',
      'build/**',
      'coverage/**',
      'src/api/generated/**',
      'src/api/schema.ts',
      '*.config.js',
      // Standalone Node tooling (e.g. the Playwright screenshot harness) lives
      // outside the typed `src/` TS project, so the type-aware lint rules can't
      // apply to it.
      'scripts/**',
      // Playwright e2e specs + config live outside the typed `src/` project and
      // are transpiled by Playwright's own runner, not the app TS program.
      'e2e/**',
      'playwright.config.ts',
    ],
  },

  eslint.configs.recommended,
  tseslint.configs.strictTypeChecked,
  tseslint.configs.stylisticTypeChecked,

  {
    files: ['**/*.{ts,tsx}'],
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: 'module',
      // Type-aware linting — REQUIRED for strictTypeChecked's no-unsafe-* rules.
      // `vite.config.ts` lives outside the app `tsconfig` (which includes only
      // `src/`); allow it as a default-project file so it is still type-aware
      // linted without polluting the app's strict program.
      parserOptions: {
        projectService: {
          allowDefaultProject: [
            'vite.config.ts',
            'vitest.config.ts',
            'lingui.config.ts',
          ],
        },
        tsconfigRootDir: import.meta.dirname,
      },
    },
    plugins: {
      'react-hooks': reactHooks,
      'react-refresh': reactRefresh,
      'jsx-a11y': jsxA11y,
    },
    rules: {
      ...reactHooks.configs.recommended.rules,
      // Static JSX accessibility checks at author time (accessibility.md).
      ...jsxA11y.flatConfigs.recommended.rules,
      'react-refresh/only-export-components': ['warn', { allowConstantExport: true }],

      // --- Absolute typing: no untyped escape hatches (made explicit). ---
      '@typescript-eslint/no-explicit-any': 'error',
      '@typescript-eslint/no-non-null-assertion': 'error', // bans the `!` operator
      '@typescript-eslint/no-unsafe-type-assertion': 'error',
      '@typescript-eslint/consistent-type-assertions': [
        'error',
        { assertionStyle: 'as', objectLiteralTypeAssertions: 'never' },
      ],
      '@typescript-eslint/ban-ts-comment': [
        'error',
        {
          'ts-ignore': true, // forbidden — use @ts-expect-error
          'ts-nocheck': true,
          'ts-check': false,
          'ts-expect-error': 'allow-with-description',
          minimumDescriptionLength: 10,
        },
      ],
      // Enforce `import type` so verbatimModuleSyntax in tsconfig stays happy.
      '@typescript-eslint/consistent-type-imports': 'error',
      // No floating/unhandled promises in a React app.
      '@typescript-eslint/no-floating-promises': 'error',
      '@typescript-eslint/no-misused-promises': 'error',
    },
  },

  // Shared modules (UI primitives, providers, hooks, route/nav tables) legitimately
  // co-export components alongside variants, contexts, hooks, and constants — the
  // shadcn/ui and React-context conventions. `react-refresh/only-export-components`
  // is a DX-only Fast-Refresh hint, not a correctness or a11y rule; disable it for
  // these non-page modules rather than fragmenting each primitive into two files.
  {
    files: [
      'src/components/ui/**/*.{ts,tsx}',
      'src/theme/**/*.{ts,tsx}',
      'src/i18n/**/*.{ts,tsx}',
      'src/realtime/**/*.{ts,tsx}',
      'src/app/navigation.tsx',
      // Test render helpers co-export a provider component + a render function;
      // Fast Refresh does not apply to test infra.
      'src/test/**/*.{ts,tsx}',
    ],
    rules: {
      'react-refresh/only-export-components': 'off',
    },
  },

  // Tests may mock — relax the strictest type-flow rules ONLY here, and document why.
  // Note: this does NOT permit weakening assertions; see agent-guardrails.md §B.2.
  {
    files: ['**/*.{test,spec}.{ts,tsx}', '**/__tests__/**/*.{ts,tsx}'],
    rules: {
      '@typescript-eslint/no-non-null-assertion': 'off',
      '@typescript-eslint/no-unsafe-assignment': 'off',
      '@typescript-eslint/no-unsafe-member-access': 'off',
      '@typescript-eslint/no-unsafe-call': 'off',
      '@typescript-eslint/no-unsafe-argument': 'off',
      // Tests narrow mocked HTTP bodies to their known request shape; this is the
      // same category as the unsafe-* relaxations above (mocking only).
      '@typescript-eslint/no-unsafe-type-assertion': 'off',
    },
  },
);
