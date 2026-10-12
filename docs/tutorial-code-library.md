# Tutorial: The code library — write a helper once, use it everywhere

Two applications both need "this endpoint is for signed-in people", and both need to hash a
share password. Copying the same ten lines into every code query works until you change one
copy and forget the other. A **code library** is the place for code that more than one query or
trigger needs: you write it once, every code body imports it with `lib("name")`, and an edit
reaches all of them on their next call.

This continues from [tutorial-rest-queries.md](tutorial-rest-queries.md) step 11: you know how
to add a JavaScript **code query** to an application's REST API. You need two applications;
the names below are `todo` and `blog`, but any two will do.

## The rule in one line

> Logic that one table rule can express goes in the table; logic two code bodies share goes in
> a **library**; everything else stays in the query.

A library is a body of JavaScript that assigns what it shares to `exports` (or
`module.exports`). It is **instance-wide** (every application and every trigger can import it)
or belongs to **one application** (only that application's queries can, and its library of a
name wins over an instance-wide one of the same name).

## Step 1 — An instance library called `auth`

Open **Settings → Development** and press **Open the code library**. Press **New library**,
name it `auth`, leave the scope at **Instance-wide**, and write:

```js
function requireUser() {
  if (!user) throw httpError(401, "Sign in to continue");
  return user;
}

async function hashShare(password) {
  if (typeof password !== "string" || password.length < 8) {
    throw httpError(400, "a share password needs at least 8 characters");
  }
  return await crypto.password.hash(password);
}

module.exports = { requireUser, hashShare };
```

Press **Save**. The server compiles the code before it stores it: a typo is refused with its
line number, and nothing is saved until it compiles. Compiling does not run anything.

Two things about `user` and the other names in there:

- A library has **no `user` of its own**. When a query imports it, `user` is the caller of
  *that* query, and `db`, `fs` and `fetch` are that query's own, with its authority. A library
  can never do more than the code that imported it.
- The library's top level runs **once per call** of the importing query, however many times
  that query (or another library) imports it. Nothing is kept from one call to the next, so a
  variable at the top of a library is not a cache shared between users.

## Step 2 — Use it from two applications

In the `todo` application, add a code query **`me`**: `GET`, sub-path `/me`, minimum role
`Public (100)`, language JavaScript:

```js
const { requireUser } = lib("auth");
const me = requireUser();
return { email: me.email };
```

In the `blog` application, add **`newShare`**: `POST`, sub-path `/shares`, minimum role
`Public (100)`:

```js
const { requireUser, hashShare } = lib("auth");
requireUser();
return { hash: await hashShare(body.password) };
```

Call them from each React app's generated client — `api.me()` and
`api.newShare({ password })`. Signed out, both answer **401** with
`{ "error": "Sign in to continue" }`; the client's error carries the message. Signed in, `me`
answers your email and `newShare` an argon2id hash. Back on the Code library screen, `auth`
now says it is **used by** `query me in todo` and `query newShare in blog`.

The editor completes `lib("…")` with the libraries that exist, so a misspelt name is a red
squiggle rather than a 500. At run time it is an error that lists what the code can import.

## Step 3 — Change it once

Edit `auth` and change `"Sign in to continue"` to `"Please sign in first"`. Save. Call both
endpoints signed out again: both answer `{ "error": "Please sign in first" }`. There is no
build and no restart. Each query picks the new code up on its next call.

## Step 4 — One application's own version

Sometimes one application needs a different rule. Make a library also called `auth`, but set
its scope to **App: blog**. From then on `blog`'s queries import that one, and `todo` keeps the
instance-wide `auth`. Delete the application's copy and `blog` falls back to the instance's.

A **trigger** belongs to no application, so it only ever sees instance-wide libraries,
whichever way it is fired. That way a trigger body means the same thing everywhere.

## Step 5 — From a coding agent

An agent connected over MCP has three tools for this:

- `describe_code_libraries` lists every library with its code, its scope and its `used_by`;
- `save_code_library` (`name`, `code`, optional `description` and `application`) creates one or
  replaces the code of the one with that name, and needs the `allow_edit` grant;
- `delete_code_library` needs `allow_drop`.

`describe_code_api` documents `lib()` beside `db` and `fs`, and every scaffolded app's
`AGENTS.md` says to put shared code in a library instead of copying it.

## Things that trip people up

- **`require` does not work.** A code body is not a Node module, and nothing is installed into
  it. `require(…)` throws and tells you to use `lib()`.
- **Libraries may import libraries**, but not in a circle: `a` importing `b` importing `a`
  throws `code libraries import each other in a cycle: a → b → a`.
- **`lib` is synchronous.** It returns the exports directly. What a library exports may be
  `async`, and you `await` those calls as usual.
- **Deleting a library breaks its importers.** The screen asks first when something uses it.
  The **used by** list comes from the code containing `lib("auth")` literally, so a name built
  at run time is not found.
- **Backups** carry libraries (`code-libraries.json`). On restore, a library whose application
  is not on the new server is skipped with a reason, and one that already exists there under
  the same name is kept.
- **JavaScript only.** A Python body has no `lib`.

## What next

[tutorial-triggers.md](tutorial-triggers.md) step 5 shows what a code body can reach (`db`,
`fs`, `fetch`, `trigger`). A library can reach all of them through the code that imports it.
