# The `code` framework: bring your own build

The [React tutorial](tutorial-react-todo.md) covers the path most applications should take:
Saltcorn creates the project, generates a typed client and hooks for your tables, installs the
dependencies and builds it. This document covers the other framework — **`code`** — which makes
no assumptions at all.

Use it when React's conventions are the wrong ones: a Next.js or SvelteKit app, a project that
already exists, a bundler that is not Vite, a layout that is not `<project>/dist`, or a
front end that is not React. Saltcorn's side of the arrangement shrinks to: run your build
command, serve what it emits, and generate a typed API client if you want one.

**What you give up**, relative to `react`: nothing is scaffolded, nothing is installed for you,
there are no generated hooks, and you state five settings instead of two, keeping them
consistent yourself. You need a shell on the server host (or another way to put files in the
store) to create the project in the first place.

## Prerequisites

Start the server with a base domain, so applications have somewhere to be served:

```bash
feldspar serve --base-domain localhost
```

## Step 1 — Add a file store and a data model

Both in the admin UI, exactly as in the React tutorial:

- **File stores → New file store**: name `apps`, backend `local`, **Directory** `/srv/apps`, and
  tick **Create the directory if it does not exist** if it is not there yet. It connects
  immediately, with no restart. (`--file-store apps=/srv/apps` at startup does the same thing
  and is handy for scripted deployments.)
- **Tables → New table name** `tasks` → **Create table**, then open it and add:
  - `title` — SQL type `text`, not nullable
  - `done` — SQL type `boolean`, nullable

## Step 2 — Create the project yourself

On the server host, inside the store directory:

```bash
cd /srv/apps
npm create vite@latest todo -- --template react-ts
cd todo && npm install && git init
```

You now have `/srv/apps/todo`, building to `/srv/apps/todo/dist`. Any toolchain works here; the
only requirement is that your build command emits static files into a directory, and that an
`index.html` is among them if you want deep links to resolve.

## Step 3 — Register the application

**Applications → New application**, choosing **Code (bring your own build)**:

| Field | Value |
|---|---|
| Name | `Todo` |
| Subdomain | `todo` |
| Framework | **Code (bring your own build)** |
| **File store** | `apps` |
| **Source directory** | `todo` |
| **Output directory** | `todo/dist` |
| **Build command** | `npm run build` |
| **Generated client path** | `todo/src/client.ts` |
| Tables | `tasks` |
| APIs (Provider / Mount) | `rest` / `/api` |

Every path is relative to the file store, and they must agree with each other and with the
project you created — that consistency is the part the `react` framework takes off your hands.
The build command is split on whitespace and run directly (no shell), so pipes, globs and quoted
arguments do not work; put anything more involved in a script or an npm script and name that.

`rest` at `/api` projects each declared table into four endpoints — `GET/POST /api/tasks`,
`PUT/DELETE /api/tasks/{id}` — plus `/api/login`, `/api/logout` and `/api/whoami`. Save.

Leave **Generated client path** empty if you do not want a generated client; the app is then an
ordinary static bundle that can call the API however it likes.

## Step 4 — Build

Press **Build** on the app's row. The server emits the typed client (if you asked for one), runs
your build command in the source directory, and serves the output directory with an SPA fallback
to `index.html`. The banner carries your bundler's log, or its diagnostics if it fails; a failed
build leaves the previously built version serving.

**Dependencies are yours to install.** Unlike a scaffolded React app, `code` runs no
`npm install`: if the build needs it, run it on the host, or make your build command do it.

The same build runs from a terminal as `feldspar build-app todo`, which prints your bundler's
output whole and mounts nothing — useful in a deploy script, or when a failed build has left the
app unreachable.

## Step 5 — Use the generated client

The generated `client.ts` is a typed factory. Endpoint paths already include the API mount, so
no base URL is needed:

```tsx
// src/App.tsx
import { useEffect, useState } from "react";
import { createClient } from "./client";

const api = createClient();

export default function App() {
  const [tasks, setTasks] = useState<any[]>([]);
  const [title, setTitle] = useState("");

  const refresh = async () => setTasks(await api.listTasks());
  useEffect(() => { refresh(); }, []);

  const add = async () => {
    await api.createTasks({ title, done: false });
    setTitle("");
    refresh();
  };

  return (
    <div>
      <input value={title} onChange={(e) => setTitle(e.target.value)} />
      <button onClick={add}>Add</button>
      <ul>
        {tasks.map((t) => (
          <li key={t.id}>
            <input
              type="checkbox"
              checked={t.done}
              onChange={() => api.updateTasks(t.id, { title: t.title, done: !t.done }).then(refresh)}
            />
            {t.title}
            <button onClick={() => api.deleteTasks(t.id).then(refresh)}>✕</button>
          </li>
        ))}
      </ul>
    </div>
  );
}
```

Method names come from the table: `listTasks`, `createTasks`, `updateTasks(id, …)`,
`deleteTasks(id)`. If the table's access requires a signed-in role, call
`await api.login({ email, password })` first — it sets a session cookie — and `api.whoami()`
returns the current user.

The `useEffect` + `useState` + `refresh()` shape above is exactly what the React framework's
generated hooks replace, and it is a fair picture of the difference between the two frameworks:
this one hands you a typed client and stays out of the way.

## Step 6 — Open the app

Rebuild after changes (**Build** again — no server restart), then visit
`http://todo.localhost:3032`.

## Step 7 — Serve images from a file store

The bundle is your code. Images are not code: a logo, a hero shot, a folder of screenshots that
a non-developer replaces on a Tuesday. Putting those through the bundler means a rebuild every
time one changes. Instead, mount a directory of a file store under the application.

Put a file in the store — through **Files** in the admin UI, or on the host:

```bash
mkdir -p /srv/apps/media && cp ~/hero.png /srv/apps/media/hero.png
```

Then edit the application and add a row under **Static directories**:

| Mount | Store | Path |
|---|---|---|
| `/img` | `apps` | `media` |

**Store is a drop-down of the stores this application declares**, not a box to type a name into.
That subset is the **File stores** picker further up the same form, and it is a different thing
from the framework's own **File store** setting in Step 3 — so if the drop-down is empty, or does
not offer `apps`, tick `apps` there first. A directory can only serve a store the application
declares access to, and the server refuses to save one that does not, naming the store.

Save. `http://todo.localhost:3032/img/hero.png` now serves `media/hero.png`, with the content
type its extension implies and an ETag, so a second page load is a 304. Use it from a page as an
ordinary relative URL:

```tsx
<img src="/img/hero.png" alt="" />
```

Relative, not `http://todo.localhost:3032/img/hero.png` — an absolute URL baked into a component
follows the application to production as a broken link.

**No rebuild.** Drop a second file into `/srv/apps/media` and it is live on the next request: the
server is serving the store, not something the bundler copied out of it. That is the whole point
of a static directory, and it is the difference from putting the image in `public/`.

**A mount is not a grant.** It says where in the URL space the store's subdirectory appears; it
does not make everything under it public. Each request is checked against the file's own access
as the viewer's role, exactly as a download through the file manager would be, and a file the
viewer may not read is the same 404 an unknown path gets. So a mount over a store with private
files is safe; it just serves fewer of them to a stranger.

Two ordering rules follow from where the directory sits in the request path: it is matched
**after** the app's APIs and **before** the framework's SPA fallback. So a mount under an API's
sub-path (`/api/img` when `rest` is at `/api`) would never be reached, and saving one is refused
with that as the reason. Anything the directory does not claim still falls through to your
bundle.

**The application's coding agent knows.** If you built this app with an agent (see
[the agents tutorial](tutorial-agents.md)), its session header lists the mounts, and its
`list_assets_*` tool returns each file with the URL the server actually answers — so "put the
hero image on the landing page" does not need you to paste a URL into the chat.

## Notes

- **The client is regenerated on every build** and reflects the tables the *application*
  declares, not every table in the database. Adding a table to the app changes the client at the
  next build.
- **Content-Security-Policy**: a `code` app defaults to `default-src 'self'`. If your bundler
  emits inline scripts or styles, or your app loads anything cross-origin, edit the policy on the
  application — the strict default is deliberate, and loosening it is a decision worth making
  explicitly.
- **The output directory must exist after the build and must not be empty**, otherwise the build
  is reported as failed even if your command exited 0 — a bundle that produced nothing is not a
  deployable app.
- **An object-store-backed file store cannot host a build.** The bundler is a process handed a
  working directory, so the store needs a local path.
