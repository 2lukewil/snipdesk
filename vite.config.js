import { defineConfig } from "vite";

// Branches on `--mode` so `npm run build:teams` produces a Teams-flavored
// bundle. The env-var check is a safety net for tooling that can't pass
// `--mode` through cleanly (Tauri's beforeBuildCommand, CI scripts).
export default defineConfig(({ mode }) => {
  const isTeamsBuild =
    mode === "teams" || process.env.SNIPDESK_TEAMS_BUILD === "1";

  return {
    root: "src",
    publicDir: false,
    clearScreen: false,
    server: {
      port: 1420,
      strictPort: true,
      host: "localhost",
      hmr: {
        protocol: "ws",
        host: "localhost",
        port: 1421,
      },
      watch: {
        ignored: ["**/src-tauri/**"],
      },
    },
    build: {
      outDir: "../dist",
      emptyOutDir: true,
      target: "es2021",
      minify: !process.env.TAURI_DEBUG ? "esbuild" : false,
      sourcemap: !!process.env.TAURI_DEBUG,
    },
    // Substituted as raw source text — must be JSON-stringified so the
    // resulting code parses as a literal. esbuild then folds the
    // `if (TEAMS_BUILD)` branches and tree-shakes the dead path.
    define: {
      __SNIPDESK_TEAMS_BUILD__: JSON.stringify(isTeamsBuild),
    },
  };
});
