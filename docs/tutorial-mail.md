# Tutorial: Mail — a shared mailbox that turns into orders

Orders are mailed to `orders@shop.example`, and every member of staff should see them, each one as
an `orders` row with its PDF attached. Saltcorn 1's `imap_sync` checked the mailbox every five
minutes and asked you to map each mail field onto a column of your own table. The bundled mail
module does neither:

- **New mail is a row within seconds.** The module keeps an IMAP IDLE connection open and writes
  each mail as it arrives.
- **You map nothing.** The module owns four tables (`mail_accounts`, `mail_folders`,
  `mail_messages`, `mail_attachments`), and your own data refers to them.

Steps 1–5 are use case A of [TODO-imap.md](../TODO-imap.md): one mailbox, shared, append-only.
Steps 6–8 are the other half: each user's own accounts kept as a two-way **mirror** for a mail
client of your own (use case B), and sending from an account (D).

## Step 1 — Install the module

**Settings → Modules → Mail (IMAP and SMTP) → Install.** The card says what it does before you
click: it creates the four tables, runs a background service, and may connect to any host. It
needs to, because each account's server is typed into a row, not into the module's settings.

What may actually be reached is narrower than "any host", and is the module's own rule:

- **Ports:** IMAP 993 (TLS) or 143 (STARTTLS, required), SMTP 465 or 587. Nothing else, and never
  without TLS.
- **Public addresses only.** A server name that resolves to a private, loopback or link-local
  address is refused, so a user cannot point an account at your intranet. The connection goes to
  the address that was checked, and the certificate is always verified against the name.

If your mail server *is* inside your network (an Exchange on the LAN), the module's **Settings**
has two fields only an admin can fill in: **Trusted servers**, one `host` or `host:port` per
line, which may resolve to a private address (and use that port), and **Extra CA certificates**:
your internal CA, which the trusted servers' certificates are checked against instead of the
public CAs.

## Step 2 — The account

**Data → mail_accounts → Add row**:

| Field | Value |
|---|---|
| label | `orders@` |
| email | `orders@shop.example` |
| owner | *(empty: a shared account)* |
| min_role | `40` (staff and above may read its mail) |
| mode | `ingest` |
| imap_host / imap_port / imap_security | `imap.shop.example` / `993` / `tls` |
| imap_user / imap_password | `orders@shop.example` / *the password* |
| folders | `["INBOX"]` (the default) |
| processed_folder | `Processed` *(optional: where imported mail is moved)* |
| error_folder | `Import errors` *(optional: where mail that failed is moved)* |
| start_from | *(optional: ignore mail older than this)* |

`imap_password` is a [Secret field](tutorial-ownership.md): it is stored encrypted, the REST API,
GraphQL and live pages show `••••••••` in its place, and only server-side code reads it.

Save it, and within a second or two its `status` reads `connecting`, then `syncing`, then `idle`.
`mail_folders` lists the account's folders, with `role` filled in for Sent, Drafts, Trash, Junk
and Archive, and `mail_messages` fills with the mail that is already there, oldest first.

**What the status says.**

| status | meaning |
|---|---|
| `idle` | connected and up to date; `last_sync_at` says when |
| `syncing` | importing, or re-reading flags |
| `error` | something went wrong; `last_error` says what, and it reconnects by itself (5 s, doubling to 10 min) |
| `auth_failed` | the login failed three times in a row. It stops trying, because repeated bad logins can lock an account. Fix the password and save the row |
| `refused` | the server is not allowed (Step 1's rules). Change the row, or have an admin trust the server |
| `disabled` | `enabled` is off |

**Who sees the mail.** The tables carry the module's ownership rule: a shared account's mail is
readable by every user at or above its `min_role`, and a personal account's (one with an
`owner`) by its owner only. That holds for REST, GraphQL, live updates and code that uses
`db.asUser()`. A user can never make a shared account, nor change one: only an admin can.

## Step 3 — Orders from mail

Make the table the mail turns into (**Data → New table**):

| Field | Type |
|---|---|
| `message` | Key → `mail_messages` |
| `customer` | Text |
| `title` | Text |
| `pdf` | File |

And the trigger (**Triggers → New trigger**):

| Field | Value |
|---|---|
| Name | `order_from_mail` |
| Event | `A row is updated` |
| Table | `mail_messages` |
| Only if | `old.body_state === "importing" && account === 1` |
| Action | `run_js_code` |

```js
const files = await db.mail_attachments.where({ message: row.id }).rows();
const pdf = files.find((f) => f.content_type === "application/pdf");
await db.orders.insert({
  message: row.id,
  customer: row.from_addr,
  title: row.subject,
  pdf: pdf ? pdf.file : null,
});
```

(`account === 1` is the id of the `orders@` row. A mail that arrives at another account makes no
order.)

Send a mail with a PDF to `orders@`. An `orders` row appears within a second or two, with the
PDF in its `pdf` field: the same file `mail_attachments.file` holds, in the module's file store
`mail_files`.

**Why an Update trigger.** A mail is imported in two steps, because its attachment rows refer to
it: the `mail_messages` row is inserted with `body_state = "importing"`, the attachments are
stored and their rows inserted, and then `body_state` is set to `full` (or `too_large`). So:

- an **Insert** trigger on `mail_messages` fires once per mail and sees all of the mail (subject,
  sender, `body_text`, `body_html`), but **not yet its attachment rows**. Use it when the text is
  all you need;
- the **Update** trigger above, with `old.body_state === "importing"`, fires once per mail, when
  the attachments are there. Flag changes and moves later are updates too, but `old.body_state`
  is no longer `importing` then, so they do not make a second order.

Either way the trigger's `user` is the module (`{ module: "@feldspar/imap", service: "sync" }`):
the mail was written by the background service, not by a person.

**Fields of your own.** You can add fields to `mail_messages` (a `handled` flag, say). The module
keeps them through upgrades, because a module never drops or changes a field it did not declare.

## Step 4 — When a mail cannot be imported

Every mail gets a row, even one the module cannot make sense of. Such a row has `import_error`
set, and the mail is moved to the `error_folder` if the account has one. The next mail is
imported as usual: one bad mail never stops the rest.

To be told, add an Insert or Update trigger on `mail_messages` with the condition
`import_error && !old.import_error`, and a `send_email` action to whoever looks after it.

**What happens on the server.** In ingest mode, nothing in the table is ever deleted:

- a mail moved to `processed_folder` or `error_folder` keeps its row, whose `folder` and `uid`
  now say where it went;
- a mail somebody deletes on the server keeps its row, with `uid` cleared;
- if the server renumbers a folder (a new UIDVALIDITY), mail is matched to its rows again by its
  Message-ID, and nothing is imported twice.

## Step 5 — Buttons in the app

The module supplies actions. To call one from a page, make a trigger with event **None**, the
module's action, and minimum role user, and name it in the application:

| action | payload | does |
|---|---|---|
| `mail_sync_now` | `{ account_id, wait }` | sync the account now, rather than at the next notification or 2-minute poll |
| `mail_set_flags` | `{ message_ids, seen, flagged }` | mark mail read/unread, flagged/not, on the server and in the table |
| `mail_attachment` | `{ attachment_id }` | the attachment's File path, storing the file first if it is not stored yet |
| `mail_test_account` | the account's connection fields, or `{ account_id }` | log in to IMAP (and SMTP, if set) and answer `{ imap, smtp, error }`. At most five a minute per user |

Each action reads what it touches **as the user who called it**, so the ownership rule decides:
a message that user may not read is "not found", exactly as one that does not exist.

## Step 6 — Personal accounts: mirror mode

A mail client in your app needs the other shape: each user enters their own accounts, sees all
their folders, and what they do in the app (read, flag, move, delete) happens on the server too.
That is `mode = mirror`. A user saves the row themselves (their app's form inserts into
`mail_accounts` as them), with `owner` set to their own id:

| Field | Value |
|---|---|
| owner | the user's own id. The ownership rule refuses any other |
| mode | `mirror` |
| email / display_name | `ann@example.com` / `Ann Example`: the From of what she sends |
| imap_… / smtp_… | her servers. The same rules as Step 1 apply |
| body_days | `90` (the default): how much mail is fetched whole |
| save_sent | empty (see Step 8) |

Only she can read the account and its mail; a second user with accounts of their own never sees
hers, through REST, GraphQL or live updates.

**What a mirror keeps:**

- **Every folder**, listed in `mail_folders` with its `role`. INBOX is watched with IDLE, so new
  mail is a row within a second; the other folders are checked every 5 minutes, and only the ones
  whose message count or modification sequence changed are opened. Folders created or deleted on
  the server (by another mail program) appear and disappear here too.
- **Newest first.** The first sync stores the newest 50 messages of each folder, so the inbox is
  usable at once. Older mail follows in the background, 50 messages every 15 seconds
  (`backfill_uid` on the folder says how far it has got; 0 means done).
- **Bodies for `body_days`.** Mail received in the last `body_days` days is stored whole. Older
  mail is stored with its headers, flags and attachment list only: `body_state = "headers"`,
  `body_text` empty. When the user opens one, call `mail_fetch_body` (Step 7).
- **Attachments are stored when first opened** (`mail_attachment`), not on import, so a mailbox
  of large attachments does not fill the file store.
- **The server decides.** Mail deleted or moved on the server is deleted or moved here. If the
  server renumbers a folder (a new UIDVALIDITY), its rows are deleted and it is synced again.
  Your own data should therefore not rely on a mirror's rows staying, the way Step 3 relies on an
  ingest account's.

**Threads.** `thread_key` is the first Message-ID in `References`, else `In-Reply-To`, else the
message's own Message-ID, and it is indexed. Group a conversation with
`where: { account, thread_key }`, across folders: a reply in Sent has the same `thread_key` as the
question in INBOX.

## Step 7 — The client's actions

As in Step 5, each action is a trigger with event **None** that the application names. Each reads
what it touches as the caller, so another user's message is "not found".

| action | payload | does | answers |
|---|---|---|---|
| `mail_fetch_body` | `{ message_id }` | fetches a `headers` message whole, and fills its body and attachment list | `{ id, body_state, fetched }` |
| `mail_attachment` | `{ attachment_id }` | stores the file if it is not stored yet | `{ file, stored }` |
| `mail_set_flags` | `{ message_ids, seen, flagged }` | as in Step 5 | |
| `mail_move` | `{ message_ids, target }` | moves to `target`: a folder path, or `role:trash`, `role:archive`, `role:spam`, … Also on ingest accounts | `{ moved, missing, not_on_server }` |
| `mail_delete` | `{ message_ids, permanent }` | moves to Trash; deletes for good when `permanent`, or when the mail is in Trash already | `{ trashed, deleted, missing, not_on_server }` |
| `mail_create_folder` | `{ account_id, name, parent }` | makes a folder (inside `parent`, a path, if given) | `{ id, path }` |
| `mail_save_draft` | as `mail_send`, plus `replaces` | saves the message in Drafts; `replaces` (a draft's row id) is deleted | `{ id, uid, message_id, replaced }` |
| `mail_send` | Step 8 | | |

`mail_delete`, `mail_create_folder`, `mail_save_draft` and `mail_fetch_body` are for mirror
accounts; an ingest account keeps every mail and refuses them. A Trash, Drafts or Sent folder the
server does not have is created when an action first needs it.

An action's changes are on the server and in the rows when it answers, so a live list
(`useLiveRows` on `mail_messages`) updates without a sync. However many actions the app calls at
once, an account never has more than two IMAP connections: the service's, which the actions
queue on while it runs, and otherwise one of the actions' own at a time.

## Step 8 — Sending

`mail_send` sends from an account through its own SMTP server. Name a trigger for it (here
`send_mail`, event None, action `mail_send`) in the application, and the app posts the message to
`{api mount}/actions/send_mail`:

```js
await post("/api/actions/send_mail", {
  account_id: 7,
  to: "Shop <orders@shop.example>",           // a string, or a list of strings or { name, address }
  cc: [], bcc: "me@example.com",
  subject: "Re: Your order",
  text: "Thanks!", html: "<p>Thanks!</p>",
  in_reply_to: 123,                            // the mail_messages row answered
  draft_id: 456,                               // the draft this was, deleted once sent
  attachments: [
    { store: "uploads", path: "quotes/q-17.pdf" },   // a file of your application's
    { attachment_id: 89 },                           // a mail attachment, to forward
  ],
});
// → { message_id, id, saved, save_error, accepted, rejected }
```

- **From** is the account's `display_name <email>`, and the Message-ID is on the account's domain.
- **A reply** (`in_reply_to`) gets `In-Reply-To` and `References` from the message answered, which
  is then marked answered, on the server too.
- **Attachments** are read **as the caller**: a file they could not download cannot be attached.
  At most 10 files and 20 MB in all; at most 50 recipients.
- **A copy goes to Sent**, with its Bcc kept, and its row is answered as `id`. Gmail and
  Microsoft 365 file sent mail into Sent themselves, so for an account on those servers the copy
  is left out, unless `save_sent` says otherwise (`true` or `false` decides it for any server). A
  copy that cannot be saved does not undo the send: `saved` is false and `save_error` says why.
- **Who may send.** From a personal account, its owner. From a shared account (Step 2), a user at
  or above its `send_role`; an admin only, when that is empty. `mail_save_draft` follows the same
  rule.

Errors name the server and never the password. To send automatically from an account (an order
confirmation from `orders@`, use case C), use the built-in `send_email` action with its SMTP
settings filled from the account: see [tutorial-triggers.md](tutorial-triggers.md).
