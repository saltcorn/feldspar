import { defineConfig, type Plugin } from "vitest/config";
import react from "@vitejs/plugin-react";

// One ES module exporting `mount(element, host)`: the admin imports it from the
// module's route and mounts it in its own page (TODO-app-constructor AC12.4).
// React and react-bootstrap are bundled in — the admin's own copies are not an
// API — and the admin page's Bootstrap styles already apply. `screen/` rather
// than `dist/`: the release copies the plugins without their `dist` directories.

/** The bundle written as `../screen/ui.cjs`, a CommonJS module whose export is
 * the script's text. The route that serves the script `require`s it: a module's
 * sandbox lets `require` load its own package's files, but `fs` reads only the
 * paths its `read` permission lists, and this module lists none. */
function scriptAsModule(): Plugin {
  return {
    name: "script-as-module",
    generateBundle(_options, bundle) {
      const chunk = bundle["ui.js"];
      if (!chunk || chunk.type !== "chunk") this.error("the build made no ui.js");
      delete bundle["ui.js"];
      this.emitFile({
        type: "asset",
        fileName: "ui.cjs",
        source: `module.exports = ${JSON.stringify(chunk.code)};\n`,
      });
    },
  };
}

// React's production build only for the bundle: the tests use `act`, which
// only its development build has.
export default defineConfig(({ command }) => ({
  plugins: [react(), scriptAsModule()],
  define:
    command === "build" ? { "process.env.NODE_ENV": JSON.stringify("production") } : {},
  build: {
    outDir: "../screen",
    emptyOutDir: true,
    lib: {
      entry: "src/main.tsx",
      formats: ["es"],
      fileName: () => "ui.js",
    },
  },
  test: {
    environment: "jsdom",
  },
}));
