import js from "@eslint/js";
import globals from "globals";

// Flat config (ESLint 9). The frontend is vanilla ES modules running in the Tauri
// webview, so browser globals apply. Rules are kept pragmatic: real bugs error,
// stylistic/legacy issues warn, so `npm run lint` stays signal over noise.
export default [
  {
    ignores: ["dist/**", "src-tauri/**", "node_modules/**"],
  },
  js.configs.recommended,
  {
    files: ["src/**/*.js"],
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: "module",
      globals: {
        ...globals.browser,
        // Mapbox GL is loaded via a <script> tag, so it's a runtime global.
        mapboxgl: "readonly",
      },
    },
    rules: {
      "no-unused-vars": ["warn", { argsIgnorePattern: "^_", varsIgnorePattern: "^_" }],
      "no-empty": ["warn", { allowEmptyCatch: false }],
      "no-console": "off",
      // The markdown renderer uses \x00 sentinels to stash code blocks — intentional.
      "no-control-regex": "off",
      "no-useless-escape": "warn",
      eqeqeq: ["warn", "smart"],
      "prefer-const": "warn",
    },
  },
];
