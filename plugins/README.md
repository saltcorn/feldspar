# Bundled modules

The modules in this directory are **developed here and shipped in the release
tarball**, and they are still modules: nothing in them is loaded until an admin
installs one from Settings → Modules, where they appear as a short catalog with
an Install button each.

That is the whole idea. A Saltcorn server should not carry an RSS parser, a
Markdown renderer, scikit-learn and a dozen other libraries on the chance that
an application wants one — and it should not have to carry a second front-end
framework's opinions on the chance that somebody prefers them — but an admin who
wants one should not have to know a package name,
find it on a registry, or trust it. So the *code* travels with the server and
the **dependencies do not**: `plugins/rss` is two files and a `package.json`
naming `rss-parser`, and `rss-parser` is downloaded by npm at the moment the
admin clicks Install. A server that installs none of these downloads nothing and
runs nothing extra.

## What is here

| directory | package | language | supplies |
|---|---|---|---|
| `rss/` | `@feldspar/rss` | JavaScript | a table provider: an RSS or Atom feed as a read-only table |
| `vue/` | `@feldspar/vue` | JavaScript | an application framework: build an app's UI in Vue 3 instead of React |
| `react-native/` | `@feldspar/react-native` | JavaScript | an application framework: an Expo (React Native) project served on the web, with Android and iOS apps as build targets |
| `markdown/` | `feldspar-markdown` | Python | two functions: Markdown to HTML, and a plain-text summary |
| `sklearn/` | `feldspar-sklearn` | Python | five model providers: ridge, gradient boosting, SVM, DBSCAN and t-SNE |

## The shape of one

A bundled module is an ordinary module — an npm package or a Python
distribution, installed from this directory with the same `npm install <dir>` /
`pip install <dir>` the Modules tab's "local directory" source uses — plus one
file that puts it in the catalog:

```json
// plugins/<id>/feldspar-module.json
{
  "name": "@feldspar/rss",          // the package's own name, once installed
  "language": "javascript",         // which host loads it, which manager installs it
  "title": "RSS feeds",             // the catalog card's heading
  "description": "One sentence, in the admin's words rather than the author's.",
  "supplies": ["A table provider, \"RSS feed\": …"],
  "installs": ["rss-parser"],       // downloaded at install time, not shipped
  "permissions": { "net": ["*"] }   // what it needs to work, granted by the click
}
```

`name` must be what the package calls itself — `package.json`'s `name`, or the
distribution's `name` in `pyproject.toml` — because that is what the installed
row is keyed by, and it is how the catalog knows a module is already installed.
`crates/sc-module/tests/bundled_catalog.rs` asserts the two agree, so a rename
that touches one and not the other fails the build rather than producing a
module the tab offers to install twice.

`permissions` is a **request**: a JavaScript module runs on a worker that
reaches nothing it was not granted (`crates/sc-module/src/permissions.rs`), and
the Modules tab prints this set beside the Install button so that the click
which grants it is made by someone who read it. Python modules have no
permission model at all (there is no sandbox — see §10 of the Python API), and
the tab says so rather than showing a set that would mean nothing.

## Adding one

1. `mkdir plugins/<id>` and write the package: `package.json` + `index.js` for
   JavaScript, `pyproject.toml` + a package directory for Python.
2. Write `feldspar-module.json` beside it.
3. `cargo test -p sc-module --test bundled_catalog` — it reads this directory,
   so a manifest that will not parse, a name that disagrees with the package, or
   a permission entry that is not a permission fails there.
4. Nothing else. The catalog is read from this directory at run time, the
   release tarball copies it whole, and the Modules tab lists what it finds.
