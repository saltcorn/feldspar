import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// The Analytics UI is its own bundle, served by `sc-server` under `/analytics/`
// (analytics TODO A1.14) the way the IDE is served under `/ide/`: a separate
// bundle rather than more admin screens, so that an application can mount it
// without the admin shell (A9). It routes on the URL's hash, so the server only
// ever serves this build's `index.html` for `/analytics/` and the hashed
// assets beside it.
//
// Content-hashed filenames and one CSS file, for the admin SPA's reasons (see
// `ui/admin/vite.config.ts`): a rebuilt bundle is a bundle the browser fetches
// again, and the strict CSP is satisfied by same-origin `<link>`s.
export default defineConfig({
  plugins: [react()],
  base: "/analytics/",
  build: {
    outDir: "dist",
    emptyOutDir: true,
    cssCodeSplit: false,
  },
  server: {
    // The stylesheet and the grid's paging helpers are the admin UI's own.
    fs: { allow: [".."] },
  },
});
