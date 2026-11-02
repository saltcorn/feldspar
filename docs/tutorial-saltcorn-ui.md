# Tutorial: Saltcorn UI — Saltcorn 1's views, on this server

Saltcorn 1 built applications out of **views** — a List, a Show, an Edit, a Filter over a table —
arranged on **pages**, with a menu across the top. Saltcorn UI is that experience on this server:
an application whose framework is **Saltcorn UI** owns a set of views and pages, and the server
renders them on the application's subdomain. The HTML is v1's: the view patterns are Saltcorn
1's own source code, run on the JavaScript worker inside the `feldspar` process, not a
re-implementation of them.

**Layouts are edited in Saltcorn 1's own drag-and-drop builder**: the canvas, the toolbox and
the settings panel v1 has shipped for years, vendored unedited. It edits the layout of a Show,
an Edit, a List and a Filter, and the whole of a page, and it saves shared **library
components** that many views and pages place.

There is nothing to build. No project directory, no bundler, no Build button: saving a view is
deploying it.

This tutorial goes two ways. **Part A** restores a Saltcorn 1 backup, ends with its views running,
and opens them in the builder. **Part B** starts from nothing — a table, a List, a Show, an Edit,
a Filter, a page, a menu and links that work. They are independent; read the one you need. The
library has a section of its own after them.

## Prerequisites: how you start the server

An application is served on its own subdomain, so the server needs a base domain:

```bash
feldspar serve --base-domain localhost
```

The admin UI is at `http://localhost:3032` and an application with subdomain `booksdb` at
`http://booksdb.localhost:3032` — browsers resolve `*.localhost` to 127.0.0.1.

The binary must have been built **with** its front-end bundles (the default). Two of them are
Saltcorn UI's: the view runtime, and the builder. A binary built with `SC_BUILD_ADMIN=0` or
`build-static.sh --no-ui` has neither. Every Saltcorn UI application then fails to mount, with a
line at boot naming the application and the missing bundle, and no flag supplies it afterwards
(README §6). A binary that has the view runtime but not the builder serves every application and
shows each layout as JSON, with a sentence saying why.

---

## Part A — Restore a Saltcorn 1 backup

This uses `saltcorn-v1-BooksDB.zip` from the repository's test fixtures
(`crates/sc-server/tests/fixtures/`): three tables — Books, Authors, Publishers — two books, seven
views and one page. Any v1 backup (**Settings → Backup** in Saltcorn 1) goes the same way.

### Step A1 — Restore it

**Backup → Restore**, and choose the zip. The dialog reads the archive before anything happens
and says what it is — *Saltcorn 1.7.0-alpha.1, imported* — and what is in it: the tables and
their rows, the file store, the users, the triggers, **one application**, **7 views** and **1
page**. Leave everything ticked and restore.

### Step A2 — Read the report

It is two lists, what was restored and what was not, and the second one is worth reading rather
than skimming. For BooksDB it says:

- **restored:** the three tables, their columns and rows, the file store and its file, one
  account, `application booksdb`, `0 library items into application booksdb`, `7 views into
  application booksdb`, `1 page into application booksdb`, and **`application booksdb
  serving`**. The two triggers are there too: `AddBook`, a v1 workflow, with its workflow, and
  `TrimPages`, a v1 `modify_row`. This server has no `modify_row`, so `TrimPages` is translated
  into a `run_js_code` body that does the same thing (see "What a Saltcorn 1 backup's actions
  become" in [tutorial-triggers.md](tutorial-triggers.md)).
- **not restored**, each with its reason:
  - the account came without its password — v1 hashes with bcrypt, this server with argon2id,
    so set a password on it;
  - the menu's 10 *Admin Page* and 5 *User Page* entries, which point at Saltcorn 1's own
    screens — Saltcorn UI's menu has its own Login and Logout;
  - the tag and the two plugins.

Nothing in the second list stopped the application. A view whose pattern this server does not
have (v1's `Room`), or whose table was not restored, would be a line there too, naming the view.

**One application per backup.** Saltcorn 1 had one set of views per site; here the unit is the
application, so the backup becomes an application named after its `site_name`, with every
imported table, file store and trigger in its subsets. The views are v1's configurations,
**untouched** — the server does not translate them, it runs them. The two exceptions are
bookkeeping: a library component's v1 number becomes this server's id everywhere it is placed,
and a page's old-style fixed state for an embedded view is moved into the layout, where the
builder keeps it.

**Restoring the same backup again** replaces that application's views, pages and library with
the backup's and keeps everything else you changed: its subdomain, its settings, its
Content-Security-Policy. Seven views stay seven.

### Step A3 — The application

**Applications** now lists **BooksDB**, framework **Saltcorn UI**, and — unlike a React app — no
Build button and no *Not built yet* badge. Open it: the tabs are **Settings**, **Views**,
**Pages**, **Library** and **App settings**. **Views** has the seven views, each with its
pattern, table, minimum role and a link that opens it on the subdomain. **Pages** has one,
*BooksOverview*.

### Step A4 — Open it

`http://booksdb.localhost:3032/`. BooksDB's page is not anybody's home page, so `/` is a short
document naming the pages and views there are. Follow **BooksOverview**.

Every BooksDB view and its page is `min_role` 1 — admins only, as they were in Saltcorn 1 — so
you are sent to the application's **Sign in** form, with the way back in its `dest`. Sign in as an
admin of this server; the imported account has no password until you set one.

You land on `/page/BooksOverview`: *Filter books*, with an author dropdown and a page-count range
slider, and *List Books* under it — a row per book, the author's last name joined in from
Authors, with *Show*, *Edit*, a *TrimPages* action and *Delete* on each row.

(The *Publisher* column is empty: both books in the fixture have no publisher. Add one through
*Edit* and it appears.)

### Step A5 — Use it

- **Filter.** Pick *Melville*. The list re-runs and narrows without a page load — the Filter sets
  the page's state and `saltcorn.js` reloads the list's fragment, exactly as in v1.
- **Show** on a row renders *Show Books*.
- **Edit** renders *Edit Books*, with the author and publisher as selects. Change the page count,
  **Save**, and you are back where you came from with the new value.
- **Add row** (at `/view/Edit Books` with no id) creates a book; **Delete** on its row deletes it.
- **TrimPages** takes 10% off the row's page count, as it did in v1. Open the trigger under
  **Triggers** to see the body the import wrote. It starts with a comment naming v1's
  `modify_row`, and it contains v1's `{ pages: Math.round(pages*0.9)}` unchanged.

### Step A6 — What the browser was sent

View the page's source. The markup is v1's, the scripts and stylesheets come from
`/static_assets/<version>/` — Bootstrap 5.3, jQuery, `saltcorn.js` — and the response carries:

```
content-security-policy: default-src 'self'; script-src 'self' 'unsafe-inline'
```

That `'unsafe-inline'` is the one relaxation, and it is deliberate. v1's HTML puts JavaScript in
`onclick` and `onchange` attributes all through its views, and an attribute handler cannot be
signed with a nonce. Nothing else moves: no `eval`, no other origin, `default-src 'self'`. The
policy is the framework's default and shows on the application's edit screen; one you state there
wins. (A side effect of `default-src 'self'`: v1's inline `style="…"` attributes do not apply, so
a few spacings differ from Saltcorn 1.)

### Step A7 — Open a view and a page in the builder

**Views → Show Books → Configure.** *Show Books* has one step, its layout, and the step shows
**Open in builder**, with *The layout as saved (JSON)* collapsed underneath. Press it.

The builder is a page of its own: the application and *View Show Books, Step 1 of 1 (Layout)*
across the top with **Back to configuration**, the toolbox on the left (*Text*, *Columns*,
*Field*, *Join*, *ViewLink*, *Action*, *Link*, *View*, *Card* and the rest), the imported layout
on the canvas as the subdomain renders it, and the settings of whatever is selected on the right.
Fields show a value from one of the table's rows, so you see what you are laying out.

- Drag **Join** onto the canvas, under the title. It arrives as `author.id`. In its settings,
  **Fields** opens a menu of the table's keys; hover **publisher** and pick **name**. Only tables
  in the application's **Tables** are offered.
- Press **Next »**. That is the save. It is checked the way every save is, so a layout naming an
  action the application does not declare is refused with a notice and the canvas stays as it
  was. *Show Books* has no later step, so you land back on the Views tab.

`/view/Show%20Books?id=1` now shows the publisher's name under the title.

**Pages → BooksOverview → Edit** opens the page the same way, with **Page properties** and
**Back to pages** in the top bar. Drag a **Text** above the filter, write in *Text to display* on
the right, and press **Done »**. You are back on the Pages tab, and `/page/BooksOverview` has the
text above its filter.

View this page's source too. It loads its scripts from the admin server itself and nothing from
anywhere else — CKEditor, the builder's text editor, included — and its policy is the admin UI's
strict one: no inline script, no `eval`. The one addition is that it may show images from the
application's own address, because pictures from the application's file store are shown on the
canvas.

---

## Part B — Start from nothing

### Step B1 — A table

**Tables**, type `books` in **New table name**, **Create table**. A new table has **no primary
key** — the table editor says so — and an Edit view cannot save a row without one, so the first
field is the key. Open the table and add:

- `id` — `integer`, tick **Primary key**. An integer key is generated by the database
  (`generated by default as identity`), so no form ever asks for it.
- `title` — `text`, not nullable
- `pages` — `integer`, nullable

Add a row or two from the table editor so there is something to list.

### Step B2 — The application

**Applications → New application**:

| Field | Value |
|---|---|
| Name | `Library` |
| Subdomain | `library` |
| Framework | **Saltcorn UI** |
| Site name | `Library` |
| Tables | `books` |

Leave the rest as it is, and the Content-Security-Policy box empty. **Create application**. There
is no banner and nothing to build: creating it mounted it, and `http://library.localhost:3032/`
already answers, with a document saying there is nothing here yet. (If it answers with the admin
sign-in screen instead, the application did not mount — the server log says why; see *Things that
trip people up*.)

A view may only name a table in the application's **Tables**. That is the difference from Saltcorn
1 worth knowing: views belong to an application, not to the server, so two applications can each
have a `List books` over the same table, configured differently.

### Step B3 — A List

On the application, **Views → New view**:

| Field | Value |
|---|---|
| Name | `List books` |
| Pattern | **List** |
| Table | `books` |
| Minimum role | *Admin (1)* |

**Create and configure**. v1's List has four steps — *Columns*, *Create new row*, *Default
state*, *Options* — and the first is its layout, so you land straight in the builder, with a
column per field: that is v1's own starting point. Press **Next »**. The layout is saved, and
the wizard carries on at *Create new row*, step 2 of 4. **Save view**.

Open `http://library.localhost:3032/view/List%20books`. Sign in as your admin (the view is
admin-only) and you are sent back to the list.

**A save replays the steps.** If a value is one a step's form would not accept — a number where
it wants a choice, a view it does not offer — the save is refused naming the step and the
setting, and nothing is saved.

### Step B4 — A Show and an Edit

Two more, over `books`, both *Admin (1)*:

- `Show book`, pattern **Show**. Its one step is its layout, so it opens in the builder with v1's
  default, a label and a value per field. Rearrange it if you like, and **Next »**.
- `Edit book`, pattern **Edit**. Its first step is its layout too: a form field per field. **Next
  »** takes you to *Fixed and blocked fields* (skipped when every field is on the form) and then
  *Edit options*. There, *Destination type* **View**, *Destination view* **List books**, and
  **Save view**. That is where a saved form goes.

`/view/Show%20book?id=1` shows the first book; `/view/Edit%20book?id=1` edits it and, on Save,
comes back to the list. `/view/Edit%20book` with no id is an empty form that creates a row.

### Step B5 — Links that work

**List books → Configure**. The *Columns* step opens in the builder again.

- **+ Add column**, then drag **ViewLink** into the new column's empty cell. In its settings,
  *View to link to* **Show book**. The cell shows the link as a row would.
- **+ Add column** again, a **ViewLink**, *View to link to* **Edit book**.
- **+ Add column** once more, and drag **Action** into it. Its *Action* is already **Delete**.

**Next »** saves the columns and lands on *Create new row*: *Use view to create* **Edit book**,
*Display create view as* **Link**, *Label for create* `Add a book`. **Save view**.

The list now has *Show book* and *Edit book* on every row, a *Delete* button, and an **Add a
book** link. Follow it, fill in the form, Save, and you are back on the list with the new row —
List → Edit → List, every hop a v1 link, nothing written by hand.

### Step B6 — A Filter

**Views → New view**, `Find books`, pattern **Filter**, table `books`, *Admin (1)*. A Filter is
nothing but its layout, and a new one is empty: you land on a blank canvas with the Filter's
toolbox, which has what filters are made of — *Search*, *Select*, *Toggle* — beside the usual
elements.

- Drag **Search** onto the canvas. It is a search bar.
- Drag **Action** under it, and set its *Action* to **Clear**.
- **Next »**. The Filter has no other step, so you are back on the Views tab.

A Filter shows nothing by itself: it sets the *state* of the page it is on, and every view on that
page sharing the state narrows to it. That is the next step. (**Select** is a dropdown of a
field's values, and a key field is the natural one — BooksDB's *Filter books* has one on
*author*.)

**What a search matches.** The rows where any **text** field contains the words, whatever the
case. That is Saltcorn 1's SQLite behaviour; its Postgres search matched word stems across all
the text fields joined together, so `books` also found *book*, and a phrase spanning two fields
matched. A key's summary field — an author's name, seen from books — is not searched.

### Step B7 — A page

**Pages → New page**:

| Field | Value |
|---|---|
| Name | `Home` |
| Title | `Library` |
| Minimum role | *Admin (1)* |

Leave *No menu* and *Fluid layout* unticked; they are what they say, a page without the
application's menu and one using the whole width of the window. **Create and build** opens the
new page in the builder, on an empty canvas.

- Drag **Text** onto the canvas. In its settings, write `Books` in *Text to display* and press
  **H1**.
- Drag **Columns** under it. It has two columns.
- Drag **View** into the left column. *View to show* **Find books**.
- Drag **View** into the right column. *View to show* **List books**, and leave *State* as
  **Shared** — that is what makes the list listen to the filter.
- Drag **Action** under the columns. It is **GoBack**, a button that goes back in the browser.
- **Done »** saves it and returns to the Pages tab.

`http://library.localhost:3032/page/Home`: type a word from a title into the search bar and press
Enter, and the list narrows to it; **Clear** clears it.

The Pages tab has **Edit** (the builder), **Properties** (the form you just filled in, where a
rename first shows what refers to the page) and **Delete** for each page.

### Step B8 — A menu and a home page

**Applications → Library → App settings**. Two of Saltcorn UI's settings are JSON, in v1's own
shapes:

**Menu** — v1's `menu_items`:

```json
[
  { "type": "Page", "label": "Home", "pagename": "Home", "min_role": 1 },
  { "type": "View", "label": "Add a book", "viewname": "Edit book", "min_role": 1 },
  { "type": "Link", "label": "Saltcorn", "url": "https://saltcorn.com", "min_role": 100 }
]
```

An entry is shown to a viewer whose role is at or above its `min_role` (1 is admin, 100 is
public); a `Header` entry with `subitems` makes a dropdown.

**Home page per role** — a role id to a page name:

```json
{ "1": "Home" }
```

**Save changes.** It is live immediately. `http://library.localhost:3032/` now opens *Home* for
an admin, with the menu across the top and *Logout* beside it. A role with no home page still gets
the document naming what there is.

---

## The library: shared components

A **library component** is a piece of layout with a name — a heading, a card, a row of fields —
that any view or page of the application places. Editing it inside any one of the places it is
used changes it everywhere. Current Saltcorn 1 calls these *shared components*; it is the same
thing, and the same code.

**Components belong to one application.** A component names fields, views and actions, and those
mean something only inside its application, so an application's components are not offered in
another's builder.

### Save one

In the builder, select what you want to share — a Text used as a heading, say. Open **Library** in
the left panel and press **Add**. Give it a *Name* and, if you like, an *Icon*, and press **Add**.
Two things happen: the component is saved, and the selection on the canvas **becomes** a placement
of it. Save the view or page as usual.

### Place it

Open another view or page in the builder. **Library** lists the application's *Saved components*;
drag one onto the canvas and save. The placement renders on the subdomain exactly as the layout
it stands for.

Not every component is offered everywhere. A component holding a **field** needs a row, so a page
does not offer it; one holding a search bar is offered on a page or a Filter but not in a Show.
That is Saltcorn 1's rule. To share a row of fields with a page, give the component a **slot**
instead.

### Slots

A **slot** is a hole in a component that each placement fills its own way: with a field and
how to show it, or with whatever you drop into it. Drag **Slot** from the Library panel into a
placed component, save, and every placement has its own empty slot to fill, while the rest of the
component stays shared.

### Edit it in place

Change anything inside a placed component — the heading's text, a style — and save the view or
page you are in. The change is written to the component, in the same save as the view or page, so
if the save is refused the component is unchanged too. Every other view and page placing it shows
the change from its next request.

### The Library tab

**Applications → your application → Library** lists the components: the icon, the name, and
**Used by** (*1 view, 1 page*), which opens into links to each view and page. Each has:

- **Layout**, the component's layout as JSON, read-only — components are edited in the builder;
- **Rename**, which refuses a name another component has;
- **Delete**, which first names every place that uses the component. Once it is gone, those
  places render blank where it was rather than failing, as in Saltcorn 1: a missing component
  must not take a working page down.

A view's and a page's rename and delete warnings also say which components its layout places.

---

## Who sees what

Two separate checks, and it matters that they are separate:

- A view's **minimum role** decides whether it runs at all. Below it, somebody who is not signed in
  is sent to `/auth/login` and back; somebody who is gets a page saying the view is not for their
  role.
- Every row the view reads or writes is read or written **as the viewer**, under the table's own
  read and write roles and ownership formula ([tutorial-ownership.md](tutorial-ownership.md)). A
  public List over a table whose rows are private shows each visitor only their own — the view
  being allowed does not make the rows allowed. A public view over a table the public may not read
  at all answers *This could not be shown*, naming the table: open the table's read role, or keep
  the view for signed-in roles.

What a view **draws** follows the same rules. A List's *Delete* is drawn for a viewer who may write
the table, or who owns the row under the table's ownership formula, and not for anybody else — and
the delete itself is checked again when it is posted.

Signing in and out are the application's own `/auth/login` and `/auth/logout`, with the same
accounts as the admin UI. **Offer sign-up** turns on `/auth/signup`, and new accounts get **Role of a
new account** (80 unless you change it; never an admin role).

## Actions in a view or a page

A view's action column or button runs one of three things, in this order: one of v1's built-in view
actions (*Delete*, *Save*, *Cancel*, *GoBack*, *Clear*, …); a **trigger the application declares**
(the application's Triggers card); anything else is refused — when the view is saved if its
configuration names it, when it runs otherwise.

A page's **Action** buttons are the same, with v1's page actions (*GoBack*) or the application's
triggers. A trigger button posts to `/page/<name>/action/<id>`, under the page's minimum role and
the viewer's authority, and answers with a notice; *GoBack* and *Clear* work in the browser and post
nothing.

## View patterns from a module

A Saltcorn 1 plugin whose content is view templates supplies patterns here too. Install
`@saltcorn/kanban` from **Settings → Modules** ([tutorial-modules.md](tutorial-modules.md)), and
**Kanban** is offered in **New view** beside the six built-in patterns, configured through the same
wizard, with its scripts added to the pages that render it. A plugin that writes raw SQL through
v1's `db` — `@saltcorn/mind-map` — installs and registers, and its views fail naming `db.query`:
this server does not hand a plugin SQL. The builder edits the layouts of Show, Edit, List, Filter
and pages; a plugin pattern's layout step, if it has one, is not opened in it yet.

## Things that trip people up

- **The subdomain serves the admin sign-in screen instead of the application.** The application is
  not mounted. The server log (and the application's save) says why — most often a binary built
  without the Saltcorn UI bundle.
- **A layout step shows JSON, and says the server was built without the builder.** The binary has
  the view runtime but not `ui/builder`. Everything else works; rebuild with the bundles to edit
  layouts.
- **A view is refused on save naming a table.** The table is not in the application's **Tables**.
- **The builder refuses a save with a notice.** The layout names something the application does
  not have — an action that is not one of v1's or a declared trigger, a view that is not the
  application's, a component that was deleted. The canvas keeps what you did; fix it and save
  again.
- **A saved component is not in the Library panel of a page.** It holds a field, which needs a row.
  Use a slot for the field (*The library*, above).
- **A search finds nothing for an author's name.** A search looks in the table's own text fields,
  not through keys (*Step B6*).
- **Saving an Edit view's form fails with a JavaScript error** — "Cannot read properties of
  undefined (reading 'type')", or "Field initialised with no name and no label". The table has no
  primary key. Add one (Part B, step B1); v1's Edit cannot insert or update without it.
- **An action column answers "not found" naming a trigger.** The application declares a trigger the
  server does not have — typically a v1 trigger the restore refused. Write it again under the same
  name.
- **A renamed view or page breaks links to it.** Rename shows what refers to the old name (views and
  pages that embed or link to it, menu entries, home pages, components) before you confirm, and
  changes none of them.
- **`/view/List books` is a 404 in a script.** Names may contain spaces, so encode them:
  `/view/List%20books`.

## What next

- [tutorial-ownership.md](tutorial-ownership.md) — rows a view shows per viewer.
- [tutorial-triggers.md](tutorial-triggers.md) — triggers a view's or a page's action runs.
- [tutorial-modules.md](tutorial-modules.md) — patterns from an installed plugin.
- [tutorial-i18n.md](tutorial-i18n.md) — these views' own strings, in a second language.
