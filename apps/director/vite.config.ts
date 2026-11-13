import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";

// Tauri expects a fixed dev port and no HMR interference from its own env.
export default defineConfig({
  plugins: [svelte(), tailwindcss()],
  clearScreen: false,
  build: {
    // The window is a webview we ship, not a browser we support — so the
    // build targets what Tauri actually embeds rather than shipping
    // transpiled-down output nothing here will run.
    target: "es2022",
    // **Split the heavy libraries out of the view chunks** (H-12).
    //
    // Without this the whole app was one 1.75 MB file: `xterm` appeared 79
    // times in it and `svelte-flow` throughout, so every cold start parsed
    // and compiled a terminal emulator and a DAG canvas whether or not the
    // person ever opened Run or Workflows. (The canvas went with the
    // workflow editor, and `@xyflow/svelte` with it — DIR-14.) Naming them
    // here also stops two views that share a library from each carrying a
    // copy.
    //
    // The *function* form, not the object form: Vite 8 bundles with
    // rolldown, which accepts only a callback here and fails the build
    // with "manualChunks is not a function" for the object literal every
    // rollup-era example shows.
    rollupOptions: {
      output: {
        manualChunks(id: string) {
          if (!id.includes("node_modules")) return;
          if (id.includes("@xterm")) return "xterm";
          if (id.includes("animejs")) return "anime";
        },
      },
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
});
