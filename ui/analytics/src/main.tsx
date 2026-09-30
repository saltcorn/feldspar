// The Analytics UI's entry point (analytics TODO A1.14).
//
// The stylesheet is the admin UI's vendored **Tabler** — a superset of
// Bootstrap 5's CSS — so the two bundles look alike and one colour scheme
// serves both. Imported through the bundler, so it is emitted as a same-origin
// `<link>` the strict CSP allows.
import "../../admin/src/vendor/tabler/tabler.min.css";
import "./analytics.css";

import { StrictMode } from "react";
import { createRoot } from "react-dom/client";

import { App } from "./App";

const container = document.getElementById("root");
if (!container) throw new Error("missing #root mount point");

createRoot(container).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
