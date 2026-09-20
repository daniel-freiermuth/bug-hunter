import { svelte } from "@sveltejs/vite-plugin-svelte";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [svelte()],
  server: {
    // Dev server proxies API calls to the Rust backend.
    proxy: {
      "/api": "http://127.0.0.1:8377",
    },
  },
  build: {
    // Production build outputs to ../ui/ so the Rust binary serves it
    // from the same path it always has.
    outDir: "../ui",
    emptyOutDir: true,
  },
});
