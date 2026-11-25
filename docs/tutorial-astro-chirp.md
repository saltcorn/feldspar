# A Twitter-style app in Astro: Chirp

The [Astro tutorial](tutorial-astro.md) builds a website: pages rendered on the server, forms
that post back. This one builds an **application** with the same framework, a small Twitter
called *Chirp*. People sign in, post, like posts and load more of the timeline, and a post
appears on everybody's timeline when it is written, with no reload.

It does that **without islands** and without a second API for the browser. Every piece of HTML
is rendered on the server by the same components:

- a **fragment** is a component in `src/fragments/` that the server also renders on its own,
  at `/_fragment/<Name>`. A form or link marked `data-fragment` fetches it and swaps it into the
  page;
- **`<Live>`** subscribes to a table's changes over the application's live socket and, for
  each changed row, fetches that row's fragment;
- **`<SignIn />`** and **`<SignUp />`** are forms the *server* answers, with its own second
  factor, identity providers and rules;
- a plain **`<script>`** calls the application's API through the generated client, for what
  is not a fragment.

With JavaScript off, every form and link still works: they are ordinary forms and links first.

What you need: the [Astro tutorial](tutorial-astro.md)'s Steps 1–3 (the module installed, a
file store, npm on the server), and a member role (**Roles → New role** `40`, `Member`).

## Step 1 — Two tables, a rule and a stream

**Tables → New table name** `posts`, with:

- `title` — text, not nullable
- `author` — text (the author's user id)
- `audience` — text: `everyone`, or `me` for a post only its author sees

and `likes`, with `post` — integer.

On `posts`, **Settings**: *Read* `admin`, *Write* `Member`, and the ownership formula

```
audience === "everyone" || author === user.id
```

so members read every public post and their own private ones. On `likes`, *Read* and *Write*
`Member`.

**Streams → New stream**: name `posts_live`, provider **Table changes**, table `posts`, least
role `Member`. The live socket delivers a change only to a subscriber who may read the row, by
the same formula.

## Step 2 — The application

**Applications → New application**:

| Field | Value |
|---|---|
| Name | `Chirp` |
| Subdomain | `chirp` |
| Framework | **Astro (website)** |
| File store | `apps` |
| Project directory | `chirp` |
| Tables | `posts`, `likes` |
| Streams | `posts_live` |
| API | **REST** at `/api`, with **Allow sign-up** on and **Role of a signed-up account** `40` |

The REST API is what the page's `<script>` calls and what carries the live socket
(`/api/live`). **Allow sign-up** is also what turns on `<SignUp />`: the same switch decides
every way of making an account.

## Step 3 — Signing in

#### `src/pages/login.astro`

```astro
---
import Layout from "../layouts/Layout.astro";
import { SignIn, SignUp } from "feldspar:components";
if (Astro.locals.user) return Astro.redirect("/");
---
<Layout title="Sign in to Chirp">
  <h1>Sign in</h1>
  <SignIn next="/" />
  <h2>New here?</h2>
  <SignUp next="/" repeat />
</Layout>
```

`<SignIn />` posts to `/_auth/sign-in`, which the server answers. A second factor, an
identity provider (`providers={["google"]}`), an account the application does not let in, and
"Remember me" are all handled there, exactly as for a React app's `api.login`. The page never
sees the session cookie. A wrong password comes back to this page as `?sign_in=failed`, and
the form says so. `<SignOut />` is a button that ends the session.

## Step 4 — One post, as a fragment

#### `src/fragments/PostCard.astro`

```astro
---
import { Csrf, fragmentUrl } from "feldspar:components";
const id = Number(Astro.props.id);
const post = await db.posts.asUser().where({ id }).first();
if (post && Astro.request.method === "POST") await db.likes.insert({ post: id });
const likes = post ? await db.likes.where({ post: id }).count() : 0;
---
{post && (
  <article class="post" data-key={post.id} id={`post-${post.id}`}>
    <p class="title">{post.title}</p>
    <form method="post" action={fragmentUrl("PostCard", { id: post.id })}
          data-fragment="PostCard" data-target={`#post-${post.id}`}>
      <Csrf />
      <button class="like">Like ({likes})</button>
    </form>
  </article>
)}

<style>
  .post {
    padding: 0.75rem 0;
    border-bottom: 1px solid var(--line);
  }
  .title {
    margin: 0 0 0.5rem;
  }
</style>
```

It is an ordinary component: the timeline renders it once per post. Because it is in
`src/fragments/`, it is **also** served on its own, at `/_fragment/PostCard?id=4`, with the
query as its props, rendered as the visitor who asked. A fragment can be requested by anybody
who knows its address, so it reads with `asUser()`, as a page must.

The like button is a form that posts to the fragment. With JavaScript on, `data-fragment` turns
it into a fetch, and the card is swapped for the fragment's answer, with the new count. With
JavaScript off, the browser posts the form, the fragment records the like, and the server sends
the browser back to the page it was on. The CSRF check is a page's.

## Step 5 — The timeline

#### `src/fragments/Timeline.astro`

```astro
---
import { paginate } from "feldspar:components";
import PostCard from "./PostCard.astro";
const { rows, next } = await paginate(db.posts.asUser(), { after: Astro.props.after, limit: 3 });
---
{rows.map((post) => <PostCard id={String(post.id)} />)}
{next && (
  <a id="more" href={`/?after=${next}`} data-fragment="Timeline" data-query={`after=${next}`}>
    Load more
  </a>
)}
```

#### `src/pages/index.astro`

```astro
---
import Layout from "../layouts/Layout.astro";
import { Live, SignOut, paginate } from "feldspar:components";
import PostCard from "../fragments/PostCard.astro";
const me = Astro.locals.user;
if (!me) return Astro.redirect("/login");
const after = Astro.url.searchParams.get("after");
const { rows, next } = await paginate(db.posts.asUser(), { after, limit: 3 });
---
<Layout title="Chirp">
  <p>Signed in as {me.email}.</p>
  <SignOut />

  <form id="compose" data-me={me.id}>
    <input name="title" required placeholder="What's happening?" />
    <select name="audience">
      <option value="everyone">Everyone</option>
      <option value="me">Only me</option>
    </select>
    <button>Post</button>
  </form>

  <Live stream="posts_live" fragment="PostCard" id="timeline">
    {rows.map((post) => <PostCard id={String(post.id)} />)}
    {next && (
      <a id="more" href={`/?after=${next}`} data-fragment="Timeline" data-query={`after=${next}`}>
        Load more
      </a>
    )}
  </Live>
</Layout>

<script>
  import { api } from "../lib/api";

  // Posting is a call to the application's API, as the signed-in visitor. The
  // client sends the CSRF header itself.
  const form = document.querySelector<HTMLFormElement>("#compose")!;
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const data = new FormData(form);
    await api.posts.create({
      title: String(data.get("title")),
      audience: String(data.get("audience")),
      author: form.dataset.me!,
    });
    form.reset();
  });

  // Any script can follow the same changes the timeline follows.
  api.posts.changes.subscribe({}, {
    element: ({ value }) => {
      if (value.op === "insert") document.body.dataset.lastPost = String(value.key);
    },
  });
</script>
```

Open `http://chirp.localhost:3032/login`, sign up, and post from two browsers:

- **`paginate`** reads one page of a table query, newest first, after the key `after`, and
  answers the rows and the `next` key. It pages by key rather than by offset, so a post written
  while you read the timeline is neither repeated nor skipped. The page reads `after` from its
  URL and the fragment reads it from its props, so "Load more" works as a link without JavaScript
  and as a fetch with it. With a fetch, `Timeline` replaces the link with the next three posts
  and a new link. The link is inside `<Live>`, so the posts it loads are kept current too.
- **`<Live>`** wraps the posts the page rendered. When a post is written, the socket says which
  row changed, and the page fetches that row's `PostCard`, as you, and puts it at the top. An
  edit replaces the card in place, a delete removes it, and a private post reaches only its
  author, because the socket checks each subscriber's right to read the row. Each card's
  outermost element has `data-key={post.id}`, which is how `<Live>` finds it. Changes that
  arrive together are handled together, so a burst of writes is not a burst of renders.
  `notice="{n} new posts"` holds new posts behind a button instead.
- **The script** is bundled into a file under `/_astro/`, like any `<script>`, and the strict
  Content-Security-Policy stands. `src/lib/api.ts`, from the scaffold, is the typed client
  generated from the application's API before every build: `api.posts.create`,
  `api.likes.list`, `api.posts.changes`, and `LiveRows` from `src/feldspar/client.ts` for a
  list a script keeps itself.

The page gets the small built-in script for fragments (`enhance`) and `<Live>` (`live`) only
because it uses them. A page with neither gets no JavaScript at all.

## How it fits together

| piece | rendered by | as |
|---|---|---|
| a page | the site's code body | the visitor |
| a fragment, `/_fragment/<Name>` | the same body | the visitor, with the page's CSRF check |
| `/_auth/sign-in`, `/_auth/sign-up`, `/_auth/sign-out` | the server | nobody until signed in |
| `/api/…`, `/api/live` | the REST API | the visitor |

And what is deliberately not here: islands (`client:*`), a client-side router and view
transitions. A page is HTML from the server, and changes to it are HTML from the server too.
A timeline built that way stays fast to load, readable by search engines, and usable without
JavaScript, and it is never rendered by two different pieces of code.
