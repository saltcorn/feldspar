# Tutorial: Mail — a shared mailbox that turns into orders

Orders are mailed to `orders@shop.example`, and every member of staff should see them, each one as
an `orders` row with its PDF attached. Saltcorn 1's `imap_sync` checked the mailbox every five
minutes and asked you to map each mail field onto a column of your own table. The bundled mail
module does neither:

- **New mail is a row within seconds.** The module keeps an IMAP IDLE connection open and writes
  each mail as it arrives.
- **You map nothing.** The module owns four tables (`mail_accounts`, `mail_folders`,
  `mail_messages`, `mail_attachments`), and your own data refers to them.

This is use case A of [TODO-imap.md](../TODO-imap.md): one mailbox, shared, append-only. A
personal mail client (mirror mode, sending) comes with M9 and gets its own sections here.

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
