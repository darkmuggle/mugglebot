import { defineConfig } from "vite";
import solid from "vite-plugin-solid";

export default defineConfig({
  plugins: [solid()],
  server: { port: 5173 },
  build: {
    // Terser, not the default esbuild, and the terminal is why.
    //
    // `@xterm/xterm` ships an already-minified ESM bundle, and esbuild re-minifying it breaks
    // `requestMode` — the DECRQM handler — with `ReferenceError: i is not defined`. Its
    // parameter is named `i` and it contains an enum IIFE, and the second mangling pass loses
    // that binding. Nothing is wrong at build time; it fails at runtime, the first time an
    // application asks the terminal about a mode.
    //
    // Which vim does immediately on startup. The exception is thrown inside xterm's write
    // pipeline, so the editor renders and then silently stops responding to keys: an edit
    // session that looks alive and ignores you. Terser mangles the same file correctly.
    //
    // Do not switch this back to esbuild without opening the Cloud page, starting an edit and
    // pressing a key. A build that succeeds proves nothing here.
    minify: "terser",
  },
});
