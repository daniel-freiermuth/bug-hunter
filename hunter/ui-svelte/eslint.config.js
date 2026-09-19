import js from "@eslint/js";
import tseslint from "typescript-eslint";
import sveltePlugin from "eslint-plugin-svelte";
import svelteParser from "svelte-eslint-parser";
import globals from "globals";

export default tseslint.config(
  js.configs.recommended,
  tseslint.configs.recommended,
  ...sveltePlugin.configs["flat/recommended"],
  {
    languageOptions: {
      globals: globals.browser,
    },
  },
  {
    files: ["**/*.svelte", "**/*.svelte.ts"],
    languageOptions: {
      parser: svelteParser,
      parserOptions: {
        parser: tseslint.parser,
      },
    },
  },
  {
    rules: {
      "@typescript-eslint/no-explicit-any": "error",
      "@typescript-eslint/no-unused-vars": ["warn", {
        argsIgnorePattern: "^_",
        varsIgnorePattern: "^_",
      }],
      // Backend HTML is trusted (rendered by our own Rust code).
      "svelte/no-at-html-tags": "off",
    },
  },
  {
    ignores: ["dist/", "node_modules/", "../ui/"],
  },
);
