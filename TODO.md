# Saltcorn v2 — Static directories: served, picked, and listed

Ordered, checkable task list for the twenty-ninth milestone after the MVP. Earlier lists are
archived in [docs/TODO-mvp.md](./docs/TODO-mvp.md) (the MVP) and
`docs/TODO-post-mvp-1.md` … [docs/TODO-post-mvp-28.md](./docs/TODO-post-mvp-28.md) (most
recently: internationalisation, and streams as an entity). Scope and rationale are in
[docs/GOALS.md](./docs/GOALS.md), whose sentence on applications is quoted in §1.

An application has images. A logo, a hero, a folder of screenshots for the docs page — bytes
that are not rows, that nobody uploads through a form, and that the person building the
application wants to point at from a page. GOALS gave them a home in the first milestone: an
application is "configured with … any number of subdirectories that are served statically", and
`Application::static_dirs` has been a stored, editable, validated-on-delete field ever since
(TODO-mvp §161–162).

**Nothing serves them.** `router.rs` matches a request against the app's API providers and then
falls through to `app.framework.handle`; `app.static_dirs` is read by `applications_using_file_store`,
by the admin API's JSON, and by nothing else. An admin can fill the form in, save it, and get a
404. So the field is a promise the server does not keep, and the agent that writes the
application's pages has no URL it could truthfully put in an `<img src>`.

This milestone keeps the promise and then tells the coding agent about it, because the two are
one job: a URL the agent can be told about is a URL something has to serve.

**Milestone definition of done:** an admin adds a static directory to an application — mount
`/img`, store picked from a **drop-down** of the stores the application declares, path `media` —
and `https://<app>/img/hero.png` serves `media/hero.png` out of that store with the right
content type and an ETag. The application's coding agent, asked to put the hero image on the
landing page, calls `list_assets_*`, is told `/img/hero.png`, and writes it into the JSX
without being told the URL by a human. The same scenario passes in `cargo test`.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# The specification

### 1. What GOALS says

> The application also is configured with the subdomain on which it is served, any number of
> APIs that are created under an application, and any number of subdirectories that are served
> statically.

Three things are configured there and two of them work. This is the third.

### 2. Where a static directory sits in the request path

An application request resolves in three steps, and the new one is the middle:

1. **An API provider**, by longest matching mount (`router.rs`, unchanged).
2. **A static directory**, by longest matching mount — new.
3. **The framework**, which serves the built bundle and whose SPA fallback claims `/*`.

The order is forced. The framework must be last because its fallback answers every path, so a
static directory behind it would never be reached. APIs must be first because an API is the
thing an application cannot work without, and an admin who mounts a directory over one should
find out at *save* time rather than by watching their data layer stop answering — which is
§5's refusal.

### 3. What is served, and to whom

The remainder of the path after the mount is resolved under `StaticDir::path` inside
`StaticDir::store`, and then read through the **same `sc_files::check_access`** every other
reader in this system goes through, as the request's user role. That is the one decision worth
stating out loud: a static directory is a **mount, not a grant**. It says where in the URL
space a store's subdirectory appears; it does not say that everything under it is public. A
file whose store or path is closed to a guest is not served to a guest, and the refusal is the
404 an unknown path gets — a 403 would confirm the file exists to somebody not allowed to know.

A path that escapes the directory (`..`) is the same 404, resolved rather than string-matched,
so there is no place to be clever about encodings.

The content type is `sc_app::asset_content_type`, already the code framework's answer for the
same question, so a `.png` in a bundle and a `.png` in a static directory are served identically.
The ETag is over the bytes, and a matching `If-None-Match` is a 304: these are images, they are
requested on every page load, and they do not change.

**Previews come free.** A preview is a `MountedApp` over the same `Application` record
(`apps.rs`), so `view_app` sees the images without anything further.

**No CSP change.** A static directory is on the application's own origin, which
`default-src 'self'` already allows. That is itself a reason to prefer this over an admin
hand-writing an `img-src` for some other host.

### 4. The URL, and why the agent is told a relative one

The public URL is `//<subdomain>.<host><mount>/<path within the directory>`. The agent is told
the **app-root-relative** part — `/img/hero.png` — for two reasons. It is what belongs in the
JSX: an absolute URL baked into a component follows the application from `localhost:3000` to
the production domain as a broken link. And it is the answer `preview_pane_url` already gives
to the same question, where the host is a `{host}` placeholder resolved in the browser
(`builder_agent.rs`), so the tree has one story about this rather than two.

### 5. The store is a pick-list, not a text field

Today `Static directories` is three text inputs — mount, store, path — and `store` is a store's
name typed from memory (`ApplicationForm.tsx`, `RepeatableRows`). This is wrong three times
over, and only the first is cosmetic:

- **The set is short, known, and already loaded.** `ApplicationForm` calls `listFileStores()`
  on mount and renders the result as a multi-select for the application's store subset, twenty
  lines above. Asking the admin to type one of those names into a box underneath is asking a
  question whose answer is on the screen.
- **A typo is silent, and stays silent.** Nothing validates the name on save — not
  `save_application`, not the admin API. A misspelled store is stored, and the first sign of it
  is a 404 from §2, or (before this milestone) nothing at all, because nothing served the
  directory either.
- **It quietly widens what the application reaches.** `StaticDir::store` is documented as
  "which should be one the app declares access to" and nothing enforces the *should*. Meanwhile
  `applications_using_file_store` counts a static directory as a reference that blocks deleting
  a store — so a typed name can pin a store an application was never granted, and an
  application's declared subset stops being the whole truth about which stores it touches.

So: the drop-down offers **the application's own declared file stores**, live from the form's
state rather than from the server's full list, and `save_application` refuses a static directory
whose store is outside the subset, naming both. The subset is right above it on the same screen,
so the fix for an empty drop-down is visible without scrolling, and the empty state says so.

A stored value the list no longer offers is still shown and still selected — the pattern the
framework picker on this same form already uses for a framework this server does not register —
because a form must never silently discard what it was given to edit.

This is TODO-post-mvp-1 §1.6's argument arriving in the one place it had not: a file-store
reference should *be* a pick-list, because being one is what carries the meaning "this is a
store" to everything downstream.

### 6. `list_assets`: what the agent gets

One read-only tool on the `coding` trait, offered when the trait's `application` setting names
an application — which an application's builder agent always sets (`builder_agent.rs`). It is
not behind `may_edit`: it reads names and sizes, nothing else. Named per scope like every other
tool the trait contributes (`list_assets_web_todo`), so an agent with two scopes has two and
neither collides.

```
list_assets_<scope>(pattern?: string, dir?: string)
→ {"assets": [
     {"url": "/img/hero.png", "path": "media/hero.png", "store": "Assets",
      "content_type": "image/png", "size": 184320, "modified": "…"}
   ], "truncated": false}
```

Newest first, capped with the same hint `find_files` gives, the same excluded directories, the
same §9 access rule as the caller. It walks the **application's** static directories, not the
coding scope: the code is in one store and the images are in another, which is exactly why
`find_files` cannot answer this. And `find_files` could not answer it anyway — a path is not a
URL, and a model handed a path will invent the URL, which is the bug.

Plus one line per static directory in the session header, beside `AGENTS.md` and the repo map
(`coding/header.rs`): `/img → store "Assets" (media), 24 files`. A model that does not know the
tool exists will not call it, and finding out costs a turn and the admin's money.

**Reading only.** Uploading an image through the agent is a grant an admin gives deliberately,
and this milestone does not invent it.

---

# The work

## Phase 1 — Serving a static directory

- [x] 1.1 `sc-server/src/router.rs`: after the API-provider match and before
      `app.framework.handle`, resolve the request against `app.static_dirs` by longest matching
      mount (§2). The remainder under `StaticDir::path` in `StaticDir::store`, read through
      `sc_files::check_access` as the request's user role; a closed file and an escaping path
      are both the 404 an unknown path gets (§3). Factor the match itself into `sc-app`
      (`Application::static_dir_for(path)`) so it is testable without a server and so the
      longest-mount rule has one implementation, as `provider_for` does.
- [x] 1.2 The response: `sc_app::asset_content_type`, an ETag over the bytes, `304` on a
      matching `If-None-Match`, and the app's CSP applied by `with_csp` like every other app
      response.
- [x] 1.3 `sc-server/tests/`: a static directory serves a file with the right content type; a
      second request with the ETag is a 304; `..` does not escape; a file closed to a guest is
      404 for a guest and served to an admin; an API mounted under the same prefix still wins;
      the framework's SPA fallback still answers a path no static directory claims.

## Phase 2 — The store is a pick-list

- [x] 2.1 `sc-app/src/store.rs`: `save_application` refuses a static directory whose store is
      not in `Application::file_stores`, naming the store and the application, beside
      `validate_api_mounts`. Refuse a mount colliding with an API mount there too (§2), for the
      reason the API-at-`/` check is already made on save.
- [x] 2.2 `ApplicationForm.tsx`: `RepeatableRows` grows a column kind — a `select` beside its
      text inputs — and `Static directories`' `store` column becomes one, its options the
      form's live `fileStores` state, a stored-but-unoffered value kept and selected, and an
      empty state that says to add a file store above (§5).
- [x] 2.3 Tests: a `sc-app` test for each refusal in 2.1; a vitest that the drop-down offers
      the declared subset, that changing the subset changes the options, and that a stored value
      outside it survives a render and a save untouched.

## Phase 3 — `list_assets` and the session header

- [x] 3.1 `sc-core-traits/src/coding/assets.rs`: the tool spec and call of §6, offered when
      `CFG_APPLICATION` is set and absent when it is not. The URL is built by the same `sc-app`
      helper Phase 1 serves through, so the tool cannot describe a URL the router does not
      answer.
- [x] 3.2 `coding/header.rs`: one line per static directory in the session header (§6), left
      out silently when the application has none or cannot be read — nothing in the header
      fails a run.
- [x] 3.3 Tests: `list_assets_*` over a fixture application returns the URL Phase 1 actually
      serves (same fixture, so the two cannot drift); the glob and the cap behave as
      `find_files`' do; a file the caller may not read is not listed; with no `application`
      setting the tool is not in the spec list; the header names the mounts.

      **Deviation:** the stable-prefix budget (8.3) went from 1 500 estimated tokens to 1 600.
      R§4's 1 500 was already spent to the last token — `act` measured 1 496 — and a tenth tool
      costs about a hundred whatever it says, so the choice was this or taking a description off
      one of the other nine. `list_assets` is the smallest spec in the set at 279 characters.
      `docs/TECHNICAL_DESIGN.md`'s sentence on the budget says so, and it is still a test.

## Phase 4 — The documentation and the walk-through

- [x] 4.1 `docs/TECHNICAL_DESIGN.md` §13.2: the request path of §2, the mount-not-a-grant rule
      of §3, and the store-subset rule of §5.
- [x] 4.2 `docs/tutorial-code-framework.md`: a short section — put an image in a store, mount
      it, use it from a page — and the note that a new file is live without a rebuild, because
      the server serves it rather than the bundler.
- [x] 4.3 Walk the definition of done by hand against a running server, and record what it
      showed. The agent half needs an API key and spends money; if that is not available, it is
      carried, and the `cargo test` half still stands.
- [x] 4.4 `CHANGELOG`.

### What running it by hand found

Walked on 2026-09-22 against a debug `feldspar serve --base-domain localhost --bind
127.0.0.1:3032 --sqlite … --file-store apps=…`, driving the admin API with `curl` as the first
admin user: an application `todo` on the `code` framework, one file store, a static directory
`/img → apps (store/media)`, and a `hero.png` on disk. Everything §3 promises happened —
`http://todo.localhost:3032/img/hero.png` served the exact bytes as `image/png` with
`etag: "bae263a2d70f2c71"`, the same request carrying that ETag was a `304`, `/img/../…` and
`/img/%2e%2e/…` were both `404`, a second file dropped into the store served **with no rebuild
and no restart**, a folder given `min_role: 1` through `setFileMeta` was `404` to a guest and
`200` to the signed-in admin, and both save refusals fired with the sentences §5 asks for (an
undeclared store, and a mount under an API's). On a second application with an API at `/api` and
a directory at `/`, `GET /api/whoami` was the **API's** `401` rather than the directory's 404,
and `GET /hero.png` was the image: the §2 order holds in the running server. Three things it
found:

1. **A static directory added by an *edit* did not serve until a build or a `SIGHUP`, and that
   is fixed.** `updateApplication` re-mounted only a *constructed* framework (Saltcorn UI);
   a built one — every `code` and `react` application — kept the `Application` record its mount
   was made with, and the router resolves static directories off that record. So the admin
   filled the form in, saved it, and the new mount's path was answered by the framework's **SPA
   fallback**: `200` with `index.html`, which is worse than the 404 this milestone set out to
   remove, because it looks like it worked. The mechanism to fix it already existed for the
   locale set — `AppMounts::refresh_mount`, which rebuilds the record and the providers and
   **keeps the bundle** — so the update handler now calls it (`refresh_mounted_app`) for every
   framework rather than re-mounting only the constructed ones. The same edit-and-it-is-live
   rule now covers the app's CSP, which was stale in exactly the same way and by the same
   sentence of §13.2. Asserted in `admin_applications_api.rs`, against the bytes rather than the
   status, because the bug's signature is a 200. Re-walked against the rebuilt server: removing
   the directory hands its path back to the SPA fallback, adding it serves the file on the very
   next request, and an edited CSP is on the next response — no build, no signal, no restart.
2. **The agent half is carried: this machine has no API key.** `POST /api/applications` said so
   itself — `no LLM provider is connected, so the `build-todo` agent that builds this
   application was not created`. What the agent would be told is pinned by
   `app_static_dirs.rs`'s `list_assets_returns_the_urls_the_router_serves`, which fetches every
   URL the tool hands the model through the real router; what is unwalked is a model reading the
   session header and writing the `<img>` itself.
3. **`feldspar serve --sqlite` still announces the config file's Postgres environment.** The
   line `database configured from the `production` environment of …/feldspar.toml` is printed
   before `--sqlite` overrides it; the server does use the SQLite file (its `_fd_*` tables were
   created there and the production database was untouched). Cosmetic, out of this milestone,
   and noted because the first reading of that line is alarming.

## Explicitly OUT of scope for this milestone

- **Uploading or writing assets from the agent.** §6. Reading is what the use case needs, and a
  write grant is a decision an admin makes on purpose.
- **Image transformation** — the `?w=1` resizing `/files/serve/` understands. A static directory
  serves the bytes that are there.
- **A directory listing.** A mount serves files, not an index; an index is a page, and a page is
  something the application's own code writes.
- **Unifying `/files/serve/` and static directories.** Saltcorn UI's route is a framework's own,
  with a store named in the path and that framework's access rules; folding the two together is
  a route change for every existing Saltcorn UI application and buys nothing this milestone
  needs.
- **Making a framework's `store` setting a pick-list** (TODO-post-mvp-1 §1.6's other half). The
  same argument applies and the fix is elsewhere: a framework declares its settings as data, so
  it needs a way to say "this one is a file store", which is a change to the settings vocabulary
  rather than to a form.

## Carried past this milestone

- From TODO-post-mvp-28: the rest of 3.6 — `de`, `es`, `zh-Hans` and `ar` for all three domains,
  and `fr` for `admin` (833 messages) and `builder` (339). One `feldspar i18n translate
  --domain D --locale L` per file, against a configured provider. **3.6 itself has the recipe**:
  the provider row the key goes in, the loop over the fifteen files, and what to do with what
  the validator refuses.
- From TODO-post-mvp-28: the non-JSX half of 3.4's sweep (see its deviations) — the `setError`
  fallback sentences, and `deleteConfirmation`/`libraryDeleteConfirmation`, which are pure
  functions in `.ts` modules whose unit tests assert the English they build.
- From TODO-post-mvp-27: the live-broker half of the streams definition of done (10.3). It needs
  a real MQTT broker, which this machine has not got; `docs/tutorial-streams.md` is the script.
- From TODO-post-mvp-26: running the agent eval against a real provider (11.4) and walking the
  agent milestone's definition of done by hand (12.3). Both need an API key and spend money;
  `docs/AGENT_EVAL.md` has the command and the heading the numbers go under.
- From TODO-post-mvp-25: page groups, HTML-file pages, copilot layout generation, uploading from
  the builder, v1's help topics, formula-editor completions, replacing CKEditor 4, a menu editor,
  cloning pages and views, sharing library items, collaborative editing, and the builder in a
  plugin pattern's mode.
- From TODO-post-mvp-24: `room`/`workflow-room` and realtime, tags, file upload from an Edit
  view, themes as plugins, a v1 `db` module for plugins, and externalising inline handlers to
  drop `'unsafe-inline'` from Saltcorn UI's CSP.
