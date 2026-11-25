# Astro websites (`plugins/astro`) — milestones W1–W3

The framework and its design are in `docs/TECHNICAL_DESIGN.md` §13.3, "Server-rendered
frameworks: Astro for websites". Tutorial: [docs/tutorial-astro.md](docs/tutorial-astro.md).

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

## W1 — The framework

- [x] W1.1 `.astro` pages compiled by `@astrojs/compiler` and bundled into one code body
  (`dist/_server/entry.js`) against a runtime of our own (`files/server.mjs`).
- [x] W1.2 The frontmatter runs as a JavaScript code body (`run_js_code`): `db`, `fetch`, `fs`,
  `trigger`, `Table`, `user`, `lib`.
- [x] W1.3 `build.server` in a framework declaration; `sc_app::server_render` serves files first,
  then runs the body; `_server/` never served; no SPA fallback.
- [x] W1.4 Opt-in prerendering after the build, through the same body.
- [x] W1.5 Scoped CSS in one hashed stylesheet; `<script>` bundled to files; assets; `public/`.
- [x] W1.6 Forms (`<Csrf />`), endpoints, `/sitemap.xml`, `/robots.txt`, the 404 page.
- [x] W1.7 Scaffold, tutorial, tests.

## W2 — Speed and polish

- [x] W2.1 A render cache: key by path (and locale), record the tables a render read, and
  invalidate on their write events. A page can opt out (`Astro.response.headers` `no-store`).
- [x] W2.2 Pass the request's cookies to the framework (`Astro.cookies.get`), excluding the
  session and CSRF cookies.
- [x] W2.3 `500.astro` for a render error, with the error still raised as an `error` event.
- [x] W2.4 Image resizing (`<Image>` from `feldspar:components`), with `width`/`height` on an
  imported image.
- [x] W2.5 A preview/dev loop: rebuild on save from the IDE without a full `npm install` check.
- [x] W2.6 Rebuild a site's prerendered pages when a table they read changes (opt-in).

## W3 — A/B optimisation of landing pages

- [x] W3.1 A visitor cookie the router reads and hands to the framework.
- [x] W3.2 `<Optimise name>` / `<Variant name?>` in `feldspar:components`: deterministic
  assignment `hash(visitor, experiment)`, crawlers always get the first variant.
- [x] W3.3 Exposure and conversion tables declared by the module; `<Goal>` (link redirect
  endpoint) and `goal(name)` for form posts.
- [x] W3.4 Thompson-sampling allocation over the counts; a fixed split as the default.
- [x] W3.5 Prerender: refuse a page with `<Optimise>`; later, one file per arm (not built:
  it needs the assignment and its exposure row in Rust).
- [x] W3.6 An Analytics workspace over the two tables.

## W4 — Interactive applications

Moved to [TODO.md](./TODO.md), "W4 — Interactive Astro applications": the generated client from a
`<script>`, sign-in components, fragments, and live fragments.
