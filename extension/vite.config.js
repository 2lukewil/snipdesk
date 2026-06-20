import { defineConfig } from "vite";
import { crx } from "@crxjs/vite-plugin";
import manifest from "./manifest.config.js";

export default defineConfig({
  plugins: [crx({ manifest })],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "es2021",
  },
  server: {
    port: 5180,
    strictPort: true,
    hmr: { port: 5181 },
  },
});
