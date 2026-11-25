# Websites with Astro: a landing page and a blog

The React and Vue frameworks build **apps**: a bundle that runs in the browser and asks the API
for data. A **website** — a landing page for an app that lives on another subdomain, a blog, a
news page, a contact form — wants the opposite: finished HTML from the server, so search engines
read it and phones show it at once, and as little JavaScript as possible.

The bundled **Astro** module does that. You write pages in Astro's `.astro` syntax; Saltcorn
compiles them and renders them **on the server**, and a page's frontmatter reads the database
exactly as a JavaScript code action does.

For an **application** built the same way — people signing in, posting, liking, a timeline
that updates by itself — see [A Twitter-style app in Astro](tutorial-astro-chirp.md) once you have
done Steps 1–3 here.

What you need: a server started with a base domain (`feldspar serve --base-domain localhost`),
a local file store (`apps`, as in the [React tutorial](tutorial-react-todo.md), Step 1), and
npm on the server.

## Step 1 — Install the module

**Settings → Modules → Astro (websites) → Install.** The module itself downloads nothing; each
site's first build installs its two build tools (`@astrojs/compiler` and `esbuild`) into its own
`node_modules`.

## Step 2 — A table for the blog

**Tables → New table name** `posts`, with:

- `slug` — text, not nullable, unique
- `title` — text, not nullable
- `body` — text (HTML)
- `published_at` — timestamp
- `published` — boolean

Add a couple of rows, one with `published` ticked.

## Step 3 — Create the site

**Applications → New application**:

| Field | Value |
|---|---|
| Name | `Acme` |
| Subdomain | `www` |
| Framework | **Astro (website)** |
| File store | `apps` |
| Project directory | `www` |
| Public site URL | `https://www.acme.example` (or blank while you try it out) |
| Tables | `posts` |

Press **Create application**: the server writes the project into the store's `www` directory.
Then press **Build** on the app's row — the first build installs the two build tools — and open
`http://www.localhost:3032`. You get:

| page | file | how it is served |
|---|---|---|
| `/` | `src/pages/index.astro` | **prerendered**: written once after the build, served as a file |
| `/posts` | `src/pages/posts/index.astro` | rendered on every request, from the table |
| `/contact` | `src/pages/contact.astro` | rendered on every request; a working form |
| anything else | `src/pages/404.astro` | the not-found page, with a 404 status |
| `/sitemap.xml`, `/robots.txt` | generated | |

The **Public site URL** is what canonical links, Open Graph tags and the sitemap use. Set it
to the address people and search engines reach the site on.

## Step 4 — Read the database in a page

Open `www/src/pages/posts/index.astro`:

```astro
---
import Layout from "../../layouts/Layout.astro";

const rows = await db.posts.limit(50).rows();
---
<Layout title="Posts" description="The latest posts.">
  <h1>Posts</h1>
  <ul class="rows">
    {rows.map((row) => <li><span>{row.title}</span></li>)}
  </ul>
</Layout>
```

The part between the `---` fences is the **frontmatter**, and it is a JavaScript code body run
on the server, with everything a code action has: `db`, `fetch`, `fs`, `trigger`, `Table`,
`user`, `lib` and `console`. So you query exactly as you would in a trigger:

```js
const posts = await db.posts
  .where({ published: true })
  .orderBy("published_at", "desc")
  .limit(10)
  .rows();
```

`Table.findOne({ name: "posts" }).getRows(...)`, Saltcorn 1's spelling, works too.

**Whose data.** Like a code action, a page reads with admin authority. That is what a public
blog wants: the visitor is anonymous and the page decides what to show (`published: true`
above). To read as the visitor instead, so the table's ownership rules apply, use
`db.posts.asUser()`; `Astro.locals.user` is the signed-in user, or `null`.

## Step 5 — One page per post

Create `www/src/pages/blog/[slug].astro`:

```astro
---
import Layout from "../../layouts/Layout.astro";

// Lists the posts for /sitemap.xml. On a page that is not prerendered, that is
// all it does: any slug still reaches the page.
export async function getStaticPaths() {
  const posts = await db.posts.where({ published: true }).rows();
  return posts.map((p) => ({ params: { slug: p.slug } }));
}

const post = await db.posts.where({ slug: Astro.params.slug, published: true }).first();
if (!post) return Astro.redirect("/404");
---
<Layout title={post.title} description={post.title}>
  <article>
    <h1>{post.title}</h1>
    <div set:html={post.body} />
  </article>
</Layout>
```

and link to it from the list: `<a href={`/blog/${row.slug}`}>{row.title}</a>`. **Build** again.

- `{post.title}` is escaped; `set:html` inserts HTML as it is, so use it only for HTML you trust.
- A new post appears the moment it is saved: the page is rendered per request.
- `/sitemap.xml` now lists every published post.

**Prerender or not?** Add `export const prerender = true` to a page and it is rendered once,
after each build, and served as a file — the fastest a page can be. For a dynamic route, it then
renders exactly the pages `getStaticPaths()` lists, and any other slug is a 404. The catch is that
it shows the data as it was at the build. So: prerender the landing page, the about page and the
legal pages; leave the blog per request (or prerender it and rebuild when you publish).

## Step 6 — The contact form

`src/pages/contact.astro` posts back to itself:

```astro
---
import Layout from "../layouts/Layout.astro";
import { Csrf } from "feldspar:components";

let error = "";
if (Astro.request.method === "POST") {
  const form = await Astro.request.formData();
  const email = String(form.get("email") || "").trim();
  const message = String(form.get("message") || "").trim();
  if (!email.includes("@") || !message) {
    error = "Please give an email address and a message.";
  } else {
    await db.messages.insert({ email, message });
    return Astro.redirect("/contact?sent=1", 303);
  }
}
---
<Layout title="Contact">
  <form method="post">
    <Csrf />
    {error && <p class="error">{error}</p>}
    <input type="email" name="email" required />
    <textarea name="message" required></textarea>
    <button>Send</button>
  </form>
</Layout>
```

Three rules make a form work:

1. **`<Csrf />`** inside the form. Every form post is checked against the visitor's CSRF cookie,
   and this is the field it is checked against.
2. **No `prerender`** on a page with a form: the token is per visitor.
3. **Redirect after the write** (`303`), so a reload does not post twice.

To send an email instead of (or as well as) storing the message, make an insert trigger on
`messages` with the `send_email` action, or call a trigger directly:
`await trigger.notify_team({ email, message })`.

## Step 7 — Components, layouts, styles and scripts

- **Components** are `.astro` files with props (`Astro.props`) and slots (`<slot />`,
  `<slot name="footer" />`); see `src/components/Feature.astro`.
- **`<style>`** in a component is scoped to that component. `<style is:global>` is not.
  `import "../styles/global.css"` in a frontmatter adds a site-wide stylesheet. All of it is
  minified into one file under `/_astro/`, which browsers cache for good.
- **`<script>`** is bundled (TypeScript and npm imports work) into a file under `/_astro/`.
  `<script is:inline>` is **blocked** by the site's Content-Security-Policy unless you loosen
  `script-src` on the application.
- **Images**: put them in `public/` and refer to them as `/hero.png`, or import one in a
  frontmatter (`import hero from "../assets/hero.png"`) — it is copied under `/_astro/` with a
  hash in its name, and the import is `{ src, width, height, format }`. To serve it resized, use
  `<Image>`:

  ```astro
  ---
  import { Image } from "feldspar:components";
  import hero from "../assets/hero.jpg";
  ---
  <Image src={hero} alt="The team at work" width={800} />
  <Image src={hero} alt="" widths={[400, 800, 1200]} sizes="100vw" format="webp" />
  ```

  Give a `width` or a `height` (the other follows the image's proportions) or both (the image is
  cropped to fill). `alt` is required: write `alt=""` for decoration. The server resizes each
  size once, on first request, and browsers keep it for good. Images are never enlarged, and an
  SVG is used as it is.
- **Endpoints**: `src/pages/rss.xml.ts` exporting `async function GET()` that returns a
  `Response` answers `/rss.xml`.
- A file or directory whose name starts with `_` in `src/pages` is not a route.

## What is different from Astro

Saltcorn uses Astro's **compiler**, not Astro. So:

| Astro | here |
|---|---|
| `astro.config.mjs`, `astro dev`, integrations | none; `npm run build` (the Build button) builds, the server serves |
| React/Vue/Svelte components, `client:*` islands | not supported — an error naming the component. Write a `<script>`, or re-render part of the page on the server with a **fragment** ([Chirp](tutorial-astro-chirp.md)) |
| `server:defer` server islands, partials (`export const partial = true`) | **fragments**: a component in `src/fragments/` is also served on its own at `/_fragment/<Name>`, and a form or link marked `data-fragment` swaps it in |
| view transitions (`transition:*`, `<ClientRouter />`) | not supported — an error naming the directive |
| sign-in left to an integration | `<SignIn />`, `<SignUp />`, `<SignOut />` from `feldspar:components`: forms the server answers, with its second factor and identity providers |
| live data needs a client framework | `<Live stream fragment>`: rows kept current by a `table_changes` stream, each change re-rendered on the server |
| Markdown/MDX pages, content collections | not supported; keep content in a table |
| `output: "static"` (prerender by default) | the opposite: **per request** by default, `prerender = true` to opt in |
| `Astro.url` is a `URL` | an object with `origin`, `pathname`, `search`, `searchParams`, `href` — the server has no `URL` class |
| `Astro.cookies.get` reads the request's cookies | the same, except the server's own session and CSRF cookies |
| `<Image />` from `astro:assets` | `<Image />` from `feldspar:components`; no `<Picture />`, no remote images resized |
| `src/pages/500.astro` | the same: a page that throws is answered with it, and `Astro.props.error` is the error |

## Speed and search engines

- Prerendered pages and everything in `public/` and `/_astro/` are served as files, with ETags;
  hashed files are cached by browsers indefinitely.
- **A prerendered page can stay current**: `export const refresh = true` beside
  `export const prerender = true` renders it again the first time it is asked for after a
  table it read was written. A blog index can be a file and still show the new post.
- **The render cache.** A page rendered for a signed-out visitor is kept and given to the next
  one until a table it read is written, so a busy page runs its frontmatter once per change
  rather than once per visit. The server knows which tables a page read because every `db` call
  goes through it. A page is not kept when it reads a cookie, a request header or the CSRF
  token (a form page), when it writes a row or runs a trigger, or when it says
  `Astro.response.headers.set("cache-control", "no-store")` — which is how to opt out a page
  that shows something no table holds, like the result of a `fetch`. Nothing is kept longer
  than ten minutes.
- Per-request pages are sent `Cache-Control: private, no-cache` unless the page says otherwise,
  because they may carry the visitor's CSRF token. A page that is the same for everyone can
  say so: `Astro.response.headers.set("cache-control", "public, max-age=60")`.
- **Errors.** Add `src/pages/500.astro` to answer a page that fails with your own layout. It
  gets the error as `Astro.props.error`; show its `message` at most. The failure is still
  raised as an `error` event, so a trigger that alerts on errors still hears about it.
- **Build on save.** In the IDE, the **Build on save** switch in the status bar runs a quick
  build whenever you save a file of the site: the bundler only, with no install and no
  prerendering (those pages render per request until the next full Build).
- The scaffolded layout writes the title, description, canonical link and Open Graph tags from
  each page's props. Give every page a `title` and a `description`.
- Each page runs within a code action's limits (5 s, and its call budget). A page making dozens
  of queries is a page to rewrite with one `.select(...)` and a Ⱶ-join.

## Step 8 — A/B test the landing page

Which headline makes more visitors sign up? Show each visitor one of two, and count.

```astro
---
// src/pages/index.astro — no `prerender` now: each visitor gets their own variant.
import Layout from "../layouts/Layout.astro";
import { Optimise, Variant, Goal } from "feldspar:components";
---
<Layout title="Acme">
  <h1>
    <Optimise name="hero-headline">
      <Variant name="best">The best tool for the job</Variant>
      <Variant name="speed">Be done in no time!</Variant>
    </Optimise>
  </h1>
  <Goal name="signup"><a class="button" href="https://app.example.com/signup">Sign up</a></Goal>
</Layout>
```

- **`<Optimise name>`** is one experiment and renders exactly one of the `<Variant>`s directly
  inside it. The choice is made on the server, so the HTML holds only that variant: no flicker,
  and no testing script. Name every `<Variant>`: the names are what is recorded.
- **Each visitor always sees the same variant.** The server gives each browser an anonymous
  visitor cookie (`sc_visitor`, a random id, a year long) the first time a page uses one, and a
  visitor's first view of an experiment is recorded in the table `astro_exposures`. Search
  engines and link previews always get the first variant and are never counted.
- **`<Goal name>`** marks success. Each link inside it goes through `/_goal/signup?to=…`, which
  records a conversion in `astro_conversions` (once per visitor) and redirects to where the
  link pointed. For a form, count the success in the frontmatter of the page it posts to:

  ```astro
  ---
  import { goal } from "feldspar:components";
  if (Astro.request.method === "POST") {
    // … store the sign-up …
    await goal("signup");
    return Astro.redirect("/thanks", 303);
  }
  ---
  ```
- **Allocation.** By default the visitors are split evenly. `weights={[80, 20]}` gives another
  fixed split. `allocate="thompson"` lets the results decide: each new visitor is shown the
  variant that a draw from each variant's conversion rate so far favours (Thompson sampling), so
  the better headline is shown more and more as the evidence builds. Add `goal="signup"` to
  optimise for one goal rather than any.
- **The results** are in the Analytics UI: the first full build of the site makes a dashboard,
  *A/B tests: <site>*, with the visitors and conversion rate of each variant and the
  conversions by goal. It is an ordinary dashboard; add a plot or a test to it like to any
  other. A difference over a few dozen visitors is usually chance.
- A page with an `<Optimise>` cannot be prerendered: the build stops and says so. It is never
  kept by the render cache either, so keep the experiment on the pages it is about.
