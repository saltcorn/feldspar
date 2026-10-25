// The Astro framework end to end, without a Saltcorn server: scaffold a
// project with the module's own generators, install its two dependencies,
// build it, and run the code body the build wrote the way the server does —
// as the body of an async function whose parameters are a code run's names —
// with a stand-in `db`.
//
//     node --test plugins/astro/test/
//
// Needs npm and the registry (or a warm npm cache). Set
// SC_ASTRO_NODE_MODULES to an existing node_modules with @astrojs/compiler and
// esbuild in it to skip the install.

import { test, before } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const astro = require("../index.js");
const framework = astro.frameworks.astro;

const project = fs.mkdtempSync(path.join(os.tmpdir(), "feldspar-astro-"));

const ctx = {
  project: "site",
  name: "site",
  runtime: "src/feldspar",
  client: "src/feldspar/client.ts",
  auth: false,
  app: { name: "Acme <Rockets>", subdomain: "acme", description: "Rockets, fast.", url: "" },
  settings: { store: "apps", project: "site", site: "https://www.acme.test" },
  tables: [
    {
      name: "posts",
      pascal: "Posts",
      title: "Posts",
      client: "posts",
      pk: "id",
      fields: [
        { name: "id", primary_key: true },
        { name: "title", primary_key: false },
      ],
    },
  ],
};

/** Write `files` (the generator's answer) under the project. */
function write(files) {
  for (const f of files) {
    const target = path.join(project, f.path);
    fs.mkdirSync(path.dirname(target), { recursive: true });
    fs.writeFileSync(target, f.contents);
  }
}

/** A `db` that answers rows of `tables` and records what was asked. Writes
 * go into `tables` too, and `groupBy(...).aggregate({ n: "count()", s:
 * "sum(f)" })` groups, which is what the A/B runtime asks. */
function fakeDb(tables) {
  const asked = [];
  let ids = 0;
  const table = (name, conds = [], group = null, agg = null, user = false) => {
    const matching = () =>
      (tables[name] || []).filter((row) => conds.every((c) => Object.entries(c).every(([k, v]) => row[k] === v)));
    const grouped = () => {
      const out = new Map();
      for (const row of matching()) {
        const key = row[group];
        const acc = out.get(key) || { [group]: key };
        for (const [alias, spec] of Object.entries(agg)) {
          const [, fn, arg] = spec.match(/^(\w+)\((\w*)\)$/);
          acc[alias] = (acc[alias] || 0) + (fn === "count" ? 1 : Number(row[arg]) || 0);
        }
        out.set(key, acc);
      }
      return [...out.values()];
    };
    const chain = {
      where: (c) => table(name, [...conds, c], group, agg, user),
      groupBy: (g) => table(name, conds, g, agg, user),
      aggregate: (a) => table(name, conds, group, a, user),
      orderBy: () => chain,
      limit: () => chain,
      asUser: () => table(name, conds, group, agg, true),
      rows: async () => {
        asked.push(user ? `${name} as the visitor` : name);
        return agg ? grouped() : matching();
      },
      first: async () => matching()[0] ?? null,
      insert: async (values) => {
        const rows = (tables[name] ||= []);
        if (name === "astro_exposures" && rows.some((r) => r.site === values.site && r.experiment === values.experiment && r.visitor === values.visitor)) {
          throw new Error("duplicate key");
        }
        rows.push({ id: ++ids, ...values });
        return ids;
      },
      update: async (values) => {
        for (const row of matching()) Object.assign(row, values);
      },
    };
    return chain;
  };
  return { db: new Proxy({ table }, { get: (t, k) => (k in t ? t[k] : table(k)) }), asked };
}

/** The smallest PNG header that says `width`×`height` (the build reads only
 * the header to size an import). */
function png(width, height) {
  const out = Buffer.alloc(33);
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]).copy(out, 0);
  out.writeUInt32BE(13, 8);
  out.write("IHDR", 12, "ascii");
  out.writeUInt32BE(width, 16);
  out.writeUInt32BE(height, 20);
  return out;
}

let body;
const run = (request, { db, user = null, trigger = {} } = {}) => {
  const AsyncFunction = (async () => {}).constructor;
  const fn = new AsyncFunction("request", "user", "db", "trigger", body);
  return fn({ query: {}, method: "GET", origin: "http://acme.localhost", ...request }, user, db, trigger);
};

before(() => {
  write(framework.scaffold(ctx));
  // Pages the scaffold does not write: the shapes a blog needs.
  write([
    {
      path: "src/pages/blog/[slug].astro",
      contents: `---
export async function getStaticPaths() {
  const posts = await db.posts.rows();
  return posts.map((p) => ({ params: { slug: p.slug } }));
}
const post = await db.posts.where({ slug: Astro.params.slug }).first();
if (!post) return Astro.redirect("/404", 302);
---
<html><head><title>{post.title}</title></head>
<body><article><h1>{post.title}</h1><div set:html={post.body} /></article>
<p class:list={["meta", { draft: !post.published }]} style={{ fontSize: "0.9rem" }}>{post.author}</p>
<script>document.body.dataset.ready = "yes";</script>
</body></html>
<style>h1 { color: rebeccapurple; }</style>
`,
    },
    {
      path: "src/pages/rss.xml.ts",
      contents: `export async function GET() {
  const posts = await db.posts.rows();
  return new Response("<rss>" + posts.map((p) => "<item>" + p.title + "</item>").join("") + "</rss>", {
    headers: { "content-type": "application/rss+xml" },
  });
}
`,
    },
    {
      path: "src/pages/_draft.astro",
      contents: "<p>not a route</p>",
    },
    {
      path: "src/pages/theme.astro",
      contents: `---
const theme = Astro.cookies.get("theme")?.value ?? "light";
---
<p class="theme">{theme}</p>`,
    },
    {
      path: "src/pages/fresh.astro",
      contents: `---
Astro.response.headers.set("cache-control", "no-store");
---
<p>always fresh</p>`,
    },
    {
      path: "src/pages/broken.astro",
      contents: `---
throw new Error("the database is on fire");
---
<p>never</p>`,
    },
    {
      path: "src/pages/500.astro",
      contents: `---
const { error } = Astro.props;
---
<h1>Something went wrong</h1><p class="why">{error.message}</p>`,
    },
    {
      path: "src/pages/latest.astro",
      contents: `---
export const prerender = true;
export const refresh = true;
const posts = await db.posts.rows();
---
<p>{posts.length} posts</p>`,
    },
    {
      path: "src/pages/landing.astro",
      contents: `---
import { Optimise, Variant, Goal } from "feldspar:components";
---
<h1><Optimise name="hero">
  <Variant>The best tool for the job</Variant>
  <Variant name="speed">Be done in <em>no time</em>!</Variant>
</Optimise></h1>
<p class="again"><Optimise name="hero"><Variant>A</Variant><Variant name="speed">B</Variant></Optimise></p>
<Goal name="signup"><a class="button" href="https://app.example.com/signup?plan=pro&ref=x">Sign up</a></Goal>`,
    },
    {
      path: "src/pages/bandit.astro",
      contents: `---
import { Optimise, Variant } from "feldspar:components";
---
<Optimise name="cta" allocate="thompson"><Variant name="a">A</Variant><Variant name="b">B</Variant></Optimise>`,
    },
    {
      path: "src/pages/signup.astro",
      contents: `---
import { goal } from "feldspar:components";
const counted = Astro.request.method === "POST" ? await goal("signup") : -1;
---
<p class="counted">{counted}</p>`,
    },
    {
      path: "src/pages/gallery.astro",
      contents: `---
import { Image } from "feldspar:components";
import hero from "../hero.png";
---
<Image src={hero} alt="A hero" width={20} />
<Image src={hero} alt="" widths={[10, 20, 400]} sizes="50vw" format="webp" />
<Image src={hero} alt="Square" width={10} height={10} />
<Image src={hero} alt="Original" />
<Image src="/logo.svg" alt="Logo" width={32} height={32} />`,
    },
  ]);
  // A 40×20 PNG, as the gallery imports it.
  fs.writeFileSync(path.join(project, "src/hero.png"), png(40, 20));
  const shared = process.env.SC_ASTRO_NODE_MODULES;
  if (shared) fs.symlinkSync(shared, path.join(project, "node_modules"), "dir");
  else execFileSync("npm", ["install", "--no-audit", "--no-fund"], { cwd: project, stdio: "inherit" });
  execFileSync("npm", ["run", "build"], { cwd: project, stdio: "inherit" });
  body = fs.readFileSync(path.join(project, "dist/_server/entry.js"), "utf8");
});

const posts = [
  { id: 1, slug: "hello", title: "Hello <world>", body: "<em>first</em>", author: "Ada", published: true },
  { id: 2, slug: "draft", title: "Draft", body: "", author: "Bo", published: false },
];

test("the build writes a server body, one stylesheet, a script file and public/", () => {
  const files = fs.readdirSync(path.join(project, "dist/_astro"));
  assert.equal(files.filter((f) => /^site\.[0-9a-f]{10}\.css$/.test(f)).length, 1, files.join());
  assert.equal(files.filter((f) => /^script\.[0-9a-f]{10}\.js$/.test(f)).length, 1, files.join());
  assert.ok(fs.existsSync(path.join(project, "dist/favicon.svg")));
  const css = fs.readFileSync(path.join(project, "dist/_astro", files.find((f) => f.endsWith(".css"))), "utf8");
  assert.match(css, /h1:where\(\.astro-[a-z0-9]+\)\{color:#639\}/, "minified and scoped");
  assert.ok(css.indexOf("box-sizing") < css.indexOf("#639"), "imported CSS comes before components'");
  assert.match(css, /:where\(\.astro-[a-z0-9]+\)/, "component styles are scoped");
  assert.ok(!/import \* as \$\$module/.test(body), "the compiler's metadata imports are gone");
});

test("a page's frontmatter reads the database with the code-action syntax", async () => {
  const { db, asked } = fakeDb({ posts });
  const out = await run({ path: "/posts" }, { db });
  assert.equal(out.status, 200);
  // The scaffolded page reads as the visitor, never at admin authority.
  assert.deepEqual(asked, ["posts as the visitor"]);
  assert.match(out.body, /^<!DOCTYPE html>/i);
  assert.match(out.body, /Hello &lt;world&gt;/, "text is escaped");
  assert.match(out.body, /<link rel="stylesheet" href="\/_astro\/site\.[0-9a-f]+\.css">/);
  assert.match(out.body, /<title>Posts<\/title>/);
  assert.match(out.body, /<link rel="canonical" href="https:\/\/www\.acme\.test\/posts"/);
  assert.match(out.body, /Acme &lt;Rockets&gt;/, "the app name is escaped into the layout");
});

test("dynamic routes, set:html, class:list, style objects and scripts", async () => {
  const { db } = fakeDb({ posts });
  const out = await run({ path: "/blog/hello" }, { db });
  assert.equal(out.status, 200, out.body);
  assert.match(out.body, /<em>first<\/em>/, "set:html is not escaped");
  assert.match(out.body, /class="meta astro-[a-z0-9]+"/);
  assert.match(out.body, /style="font-size:0\.9rem"/);
  assert.match(out.body, /<script type="module" src="\/_astro\/script\.[0-9a-f]+\.js"><\/script>/);
  assert.ok(!out.body.includes("dataset.ready"), "the script is a file, not inline");

  const missing = await run({ path: "/blog/nope" }, { db });
  assert.equal(missing.status, 302);
  assert.deepEqual(missing.headers.find(([k]) => k === "location"), ["location", "/404"]);
});

test("a form posts back with its CSRF field and redirects", async () => {
  const { db } = fakeDb({});
  const page = await run({ path: "/contact", csrfToken: "tok123" }, { db });
  assert.match(page.body, /<input type="hidden" name="_csrf" value="tok123">/);

  const bad = await run({ path: "/contact", method: "POST", form: [["email", "x"], ["message", ""]] }, { db });
  assert.equal(bad.status, 200);
  assert.match(bad.body, /Please give an email address/);

  const good = await run(
    { path: "/contact", method: "POST", form: [["email", "a@b.c"], ["message", "hi"]] },
    { db }
  );
  assert.equal(good.status, 303);
});

test("endpoints, the sitemap, robots.txt and the 404 page", async () => {
  const { db } = fakeDb({ posts });
  const rss = await run({ path: "/rss.xml" }, { db });
  assert.equal(rss.body, "<rss><item>Hello <world></item><item>Draft</item></rss>");
  assert.deepEqual(rss.headers.find(([k]) => k === "content-type"), ["content-type", "application/rss+xml"]);

  const map = await run({ path: "/sitemap.xml" }, { db });
  for (const loc of ["/", "/blog/hello", "/blog/draft", "/contact", "/posts"]) {
    assert.ok(map.body.includes(`<loc>https://www.acme.test${loc}</loc>`), `${loc} in ${map.body}`);
  }
  assert.ok(!map.body.includes("404"), map.body);

  const robots = await run({ path: "/robots.txt" }, { db });
  assert.match(robots.body, /Sitemap: https:\/\/www\.acme\.test\/sitemap\.xml/);

  const missing = await run({ path: "/no/such/page" }, { db });
  assert.equal(missing.status, 404);
  assert.match(missing.body, /Page not found/);

  const partial = await run({ path: "/_draft" }, { db });
  assert.equal(partial.status, 404, "a file starting with _ is not a route");
});

test("prerendering: the build is asked which pages, then renders each", async () => {
  const { db } = fakeDb({ posts });
  const list = await run({ mode: "prerender-list" }, { db });
  assert.deepEqual(list.paths.sort(), ["/", "/404", "/latest"]);
  assert.deepEqual(list.refresh, ["/latest"], "only the page that says refresh");
  const index = await run({ mode: "prerender", path: "/" }, { db, user: { id: 7 } });
  assert.equal(index.status, 200);
  assert.match(index.body, /Rockets, fast\./);
  const notFound = await run({ mode: "prerender", path: "/404" }, { db });
  assert.equal(notFound.status, 404);
});

test("an island fails by name rather than rendering nothing", async () => {
  write([
    {
      path: "src/pages/island.astro",
      contents: `---
import Counter from "../components/Feature.astro";
---
<Counter client:load title="x" />`,
    },
  ]);
  execFileSync("npm", ["run", "build"], { cwd: project, stdio: "pipe" });
  body = fs.readFileSync(path.join(project, "dist/_server/entry.js"), "utf8");
  await assert.rejects(run({ path: "/island" }, fakeDb({})), /islands .* are not supported/);
  fs.rmSync(path.join(project, "src/pages/island.astro"));
});

test("the render cache: an answer says whether another visitor may be given it", async () => {
  const { db } = fakeDb({ posts });
  const shared = await run({ path: "/posts" }, { db });
  assert.equal(shared.cache, true, "a page of rows is the same for everybody");
  const form = await run({ path: "/contact", csrfToken: "tok" }, { db });
  assert.equal(form.cache, false, "a page with this browser's CSRF token is not");
  const optedOut = await run({ path: "/fresh" }, { db });
  assert.equal(optedOut.cache, false, "Cache-Control: no-store opts out");
  assert.deepEqual(optedOut.headers.find(([k]) => k === "cache-control"), ["cache-control", "no-store"]);
  const missing = await run({ path: "/no/such/page" }, { db });
  assert.equal(missing.cache, true, "the 404 page is shared too");
  const redirect = await run({ path: "/blog/nope" }, { db });
  assert.notEqual(redirect.cache, true, "a redirect is not kept");
});

test("cookies reach Astro.cookies, and reading one makes the page the visitor's", async () => {
  const { db } = fakeDb({});
  const dark = await run({ path: "/theme", cookies: { theme: "dark" } }, { db });
  assert.match(dark.body, /<p class="theme[^"]*">dark<\/p>/);
  assert.equal(dark.cache, false);
  const none = await run({ path: "/theme" }, { db });
  assert.match(none.body, />light<\/p>/);
});

test("a render error is answered by 500.astro, given the error", async () => {
  const { db } = fakeDb({});
  await assert.rejects(run({ path: "/broken" }, { db }), /the database is on fire/);
  const page = await run({ mode: "error", path: "/broken", error: "the database is on fire" }, { db });
  assert.equal(page.status, 500);
  assert.equal(page.cache, false);
  assert.match(page.body, /Something went wrong/);
  assert.match(page.body, /the database is on fire/);
  const list = await run({ mode: "prerender-list" }, { db });
  assert.ok(!list.paths.includes("/500"), "the 500 page is never a file");
});

test("<Image>: an imported image carries its size, and variants point at /_image", async () => {
  const { db } = fakeDb({});
  const out = await run({ path: "/gallery" }, { db });
  assert.equal(out.status, 200, out.body);
  const imgs = out.body.match(/<img[^>]*>/g);
  assert.equal(imgs.length, 5, out.body);
  const [resized, srcset, square, original, remote] = imgs;
  assert.match(resized, /src="\/_image\?src=%2F_astro%2Fhero\.[0-9a-f]{10}\.png&amp;w=20"/);
  assert.match(resized, /width="20" height="10"/, "the height follows the aspect ratio");
  assert.match(resized, /alt="A hero" loading="lazy" decoding="async"/);
  assert.match(srcset, /srcset="[^"]*&amp;w=10&amp;f=webp 10w, [^"]*&amp;w=20&amp;f=webp 20w, [^"]*&amp;w=40&amp;f=webp 40w"/, "never larger than the image");
  assert.match(srcset, /sizes="50vw"/);
  assert.match(square, /&amp;w=10&amp;h=10"/);
  assert.match(square, /width="10" height="10"/);
  assert.match(original, /src="\/_astro\/hero\.[0-9a-f]{10}\.png" width="40" height="20"/, "unchanged is the original");
  assert.match(remote, /src="\/logo\.svg" width="32" height="32"/);

  write([{ path: "src/pages/noalt.astro", contents: `---\nimport { Image } from "feldspar:components";\nimport hero from "../hero.png";\n---\n<Image src={hero} />` }]);
  execFileSync("npm", ["run", "build"], { cwd: project, stdio: "pipe" });
  body = fs.readFileSync(path.join(project, "dist/_server/entry.js"), "utf8");
  await assert.rejects(run({ path: "/noalt" }, { db }), /<Image> needs an `alt`/);
  fs.rmSync(path.join(project, "src/pages/noalt.astro"));
});

test("A/B: one variant per visitor, kept for them, recorded once; crawlers get the first", async () => {
  const tables = {};
  const { db } = fakeDb(tables);
  const visit = (visitor, extra = {}) => run({ path: "/landing", visitor, site: "Acme", ...extra }, { db });
  const shown = (out) => (/no time/.test(out.body) ? "speed" : /best tool/.test(out.body) ? "control" : "none");

  const first = await visit("v-1");
  assert.equal(first.status, 200, first.body);
  assert.equal(first.cache, false, "a page with an experiment is the visitor's own");
  assert.equal(first.visitor, true, "the router must keep the visitor cookie");
  assert.equal((first.body.match(/best tool|no time/g) || []).length, 1, "only the chosen variant is in the HTML");
  const again = first.body.match(/<p class="again[^"]*">([AB])<\/p>/)[1];
  assert.equal(again, shown(first) === "speed" ? "B" : "A", "one experiment twice on a page shows one arm");

  for (let i = 0; i < 3; i++) assert.equal(shown(await visit("v-1")), shown(first), "the same visitor sees the same arm");
  assert.equal(tables.astro_exposures.length, 1, "an exposure is recorded once per visitor");
  assert.deepEqual(
    { site: tables.astro_exposures[0].site, experiment: tables.astro_exposures[0].experiment, converted: tables.astro_exposures[0].converted },
    { site: "Acme", experiment: "hero", converted: 0 }
  );

  // An even split, by hash: across many visitors both arms are shown.
  const arms = new Set();
  for (let i = 0; i < 40; i++) arms.add(shown(await visit(`many-${i}`)));
  assert.deepEqual([...arms].sort(), ["control", "speed"]);
  const counted = tables.astro_exposures.length;

  const bot = await visit("v-bot", { headers: { "user-agent": "Mozilla/5.0 (compatible; Googlebot/2.1)" } });
  assert.equal(shown(bot), "control", "a crawler always gets the first variant");
  const anonymous = await visit("");
  assert.equal(shown(anonymous), "control");
  assert.equal(anonymous.visitor, false, "no visitor id, nothing to keep");
  assert.equal(tables.astro_exposures.length, counted, "and neither is counted");
});

test("A/B: <Goal> links go through /_goal/, which counts once per experiment and redirects", async () => {
  const tables = {};
  const { db } = fakeDb(tables);
  const page = await run({ path: "/landing", visitor: "v-2", site: "Acme" }, { db });
  const href = page.body.match(/<a rel="nofollow" class="button[^"]*" href="([^"]+)"/)[1].replace(/&amp;/g, "&");
  assert.match(href, /^\/_goal\/signup\?to=https%3A%2F%2Fapp\.example\.com%2Fsignup%3Fplan%3Dpro%26ref%3Dx&s=[0-9a-f]{16}$/);
  const [path, qs] = href.split("?");
  const query = Object.fromEntries(qs.split("&").map((kv) => kv.split("=").map(decodeURIComponent)));
  const follow = (extra = {}) => run({ path, query, visitor: "v-2", site: "Acme", ...extra }, { db });

  const out = await follow();
  assert.equal(out.status, 302);
  assert.deepEqual(out.headers.find(([k]) => k === "location"), ["location", "https://app.example.com/signup?plan=pro&ref=x"]);
  assert.equal(out.cache, false);
  assert.equal(tables.astro_conversions.length, 1);
  assert.equal(tables.astro_conversions[0].experiment, "hero");
  assert.equal(tables.astro_conversions[0].variant, tables.astro_exposures[0].variant);
  assert.equal(tables.astro_exposures[0].converted, 1, "the exposure says it converted");
  await follow();
  assert.equal(tables.astro_conversions.length, 1, "a second click is not a second conversion");

  const forged = await run({ path, query: { ...query, to: "https://evil.example/" }, visitor: "v-2", site: "Acme" }, { db });
  assert.equal(forged.status, 200, "a link this site did not sign is not followed");
  assert.match(forged.body, /This link leads to <a rel="nofollow noopener" href="https:\/\/evil\.example\/">/);
  const local = await run({ path: "/_goal/signup", query: { to: "/thanks" }, visitor: "v-9", site: "Acme" }, { db });
  assert.equal(local.status, 302, "a path on this site needs no signature");
  assert.equal(tables.astro_conversions.at(-1).experiment, undefined, "a visitor who saw no experiment converts on none");

  // A form's success, counted from the frontmatter.
  await run({ path: "/landing", visitor: "v-3", site: "Acme" }, { db });
  const posted = await run({ path: "/signup", method: "POST", form: [], visitor: "v-3", site: "Acme" }, { db });
  assert.match(posted.body, /<p class="counted[^"]*">1<\/p>/);
});

test("A/B: Thompson sampling favours the arm that converts", async () => {
  const exposures = [];
  for (let i = 0; i < 200; i++) {
    exposures.push({ site: "Acme", experiment: "cta", variant: "a", visitor: `old-a${i}`, converted: i < 5 ? 1 : 0 });
    exposures.push({ site: "Acme", experiment: "cta", variant: "b", visitor: `old-b${i}`, converted: i < 60 ? 1 : 0 });
  }
  const tables = { astro_exposures: exposures };
  const { db } = fakeDb(tables);
  let b = 0;
  for (let i = 0; i < 40; i++) {
    const out = await run({ path: "/bandit", visitor: `new-${i}`, site: "Acme" }, { db });
    if (out.body.trim().endsWith("B")) b++;
  }
  assert.ok(b >= 36, `the better arm was shown ${b} times in 40`);
});

test("A/B: a page with an <Optimise> cannot be prerendered", async () => {
  write([
    {
      path: "src/pages/static-test.astro",
      contents: `---\nexport const prerender = true;\nimport { Optimise, Variant } from "feldspar:components";\n---\n<Optimise name="x"><Variant>a</Variant></Optimise>`,
    },
  ]);
  execFileSync("npm", ["run", "build"], { cwd: project, stdio: "pipe" });
  body = fs.readFileSync(path.join(project, "dist/_server/entry.js"), "utf8");
  await assert.rejects(
    run({ mode: "prerender", path: "/static-test" }, fakeDb({})),
    /src\/pages\/static-test\.astro has an A\/B test .* cannot be prerendered/
  );
  fs.rmSync(path.join(project, "src/pages/static-test.astro"));
  execFileSync("npm", ["run", "build"], { cwd: project, stdio: "pipe" });
  body = fs.readFileSync(path.join(project, "dist/_server/entry.js"), "utf8");
});
