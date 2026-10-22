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
client of your own (use case B), and sending from an account (D). Step 9 sends automated mail
from the shared account (C), and Step 10 builds the client's pages on the tables: a live inbox,
a message shown safely, and a reply.

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
confirmation from `orders@`, use case C), you need no module action: Step 9.

## Step 9 — Mail from your own data: "your order has shipped"

Step 3's orders came from `orders@`, and the answer to a customer should come from there too,
through that mailbox's own SMTP server, not from the installation's address in **Settings →
Email**. That is the built-in `send_email` action with its SMTP override settings
([tutorial-triggers.md](tutorial-triggers.md), "sending from an account's own server"), filled
from the account the order's mail arrived at. Add a `shipped` Bool field to `orders`, then
**Triggers → New trigger**:

| Field | Value |
|---|---|
| Name | `order_shipped` |
| Event | `A row is updated` |
| Table | `orders` |
| Only if | `shipped && !old.shipped` |
| Action | `send_email` |

and its settings:

| Setting | Value |
|---|---|
| to | `{{ messageⱵfrom_addr }}` |
| from | `{{ messageⱵaccountⱵdisplay_name }} <{{ messageⱵaccountⱵemail }}>` |
| subject | `Shipped: {{ title }}` |
| text | `Your order "{{ title }}" is on its way.` |
| smtp_host | `{{ messageⱵaccountⱵsmtp_host }}` |
| smtp_port | `{{ messageⱵaccountⱵsmtp_port }}` |
| smtp_security | `{{ messageⱵaccountⱵsmtp_security }}` |
| smtp_user | `{{ messageⱵaccountⱵsmtp_user }}` |
| smtp_password | `{{ messageⱵaccountⱵsmtp_password }}` |

Tick **shipped** on an order and the customer gets the mail from `orders@`. The template follows
two keys, `orders.message` to `mail_messages` and its `account` to `mail_accounts`, and a `{{ }}`
template is the one kind of formula that may end on a Secret field, so the password is decrypted
for the send and shown nowhere: the trigger editor stores the template, not the password. The
override is held to Step 1's rules, so the account's SMTP server must be public (465 or 587).

Two differences from `mail_send` (Step 8): `send_email` does not put a copy in Sent (Gmail and
Microsoft 365 file one themselves; other servers keep none), and it does not thread the reply.
When either matters, call `mail_send` from a code body instead (`in_reply_to: row.message`).

## Step 10 — A mail client on the tables

Steps 6–8 gave each user a mirror of their own accounts. What remains is the app: the module
supplies no pages (TODO-imap.md §2.6), because a mail client is your application's own design.
This step builds its three hard parts in the React app of
[tutorial-react-todo.md](tutorial-react-todo.md): a folder that updates by itself, a message
shown without letting it run anything or phone home, and a reply.

### Expose the tables, the streams and the actions

**Applications → your app → Edit**:

- **Tables:** tick `mail_accounts`, `mail_folders`, `mail_messages` and `mail_attachments`. The
  ownership rule (Step 2) goes with them: each user's API answers their own accounts and mail,
  and the shared accounts their role reaches.
- **Streams:** make two `table_changes` streams (**Streams → New stream**), `mail_messages_live`
  on `mail_messages` and `mail_folders_live` on `mail_folders`, with **Minimum role to observe**
  user, and tick both. A change is delivered only to subscribers who may read the row, so the
  same rule decides who hears about what.
- **Triggers:** one trigger per action, event **None**, minimum role user, named like the action
  (`mail_fetch_body`, `mail_set_flags`, `mail_attachment`, `mail_send`, and the others of Step 7
  you use), and tick them. The client then has `api.runMailFetchBody(…)`, `api.runMailSend(…)`
  and so on.

Build the app, so `src/feldspar/` is regenerated with the tables, `live` and the `run…` methods.

### A folder that updates by itself

```tsx
// src/mail/Folder.tsx
import { useLiveRows } from "../feldspar/live-react";
import { api } from "../feldspar/hooks";

const since = new Date(Date.now() - 30 * 86_400_000).toISOString();

export function Folder({ folderId, open }: { folderId: number; open: (id: number) => void }) {
  const { rows, loading } = useLiveRows(api.mail_messages, {
    where: { folder: `eq.${folderId}`, internal_date: `gte.${since}` },
    orderBy: "internal_date.desc",
    select: "id,subject,from_name,from_addr,snippet,internal_date,seen,flagged,has_attachments",
  });
  if (loading) return <p>Loading…</p>;
  return (
    <ul className="mail-list">
      {rows.map((m) => (
        <li key={m.id} className={m.seen ? "" : "unread"} onClick={() => open(m.id)}>
          <b>{m.from_name || m.from_addr}</b> {m.subject}
          {m.has_attachments && " 📎"} <span>{m.snippet}</span>
        </li>
      ))}
    </ul>
  );
}
```

New mail is a row about a second after the server has it (IDLE on INBOX; the other folders within
5 minutes), and `useLiveRows` puts it at the top of the list without a reload. So do flags set in
another mail program, mail moved or deleted there, and the user's own actions. The `where` keeps
the read to the last 30 days: `useLiveRows` reads every row its filter matches, and a mirror's
folder can hold tens of thousands. Page further back with an ordinary `api.mail_messages.list`.
The folder list is the same hook on `mail_folders` (`` where: { account: `eq.${accountId}` } ``),
whose `unread` and `total` the service keeps current.

### Showing a message safely

`body_html` is whatever the sender wrote. Show it as it is, and a mail can run scripts in your
app, submit forms, or load a remote image that tells the sender you opened it and from where.
Mail programs therefore show it in a sandbox, with remote content blocked until the reader asks.
Three layers do that here:

1. **The iframe's `sandbox`** without `allow-scripts`: nothing in the mail runs, whatever slipped
   past the rest. `allow-same-origin` lets the page measure the frame's height and lets the
   frame load the inline images from your app's API with the user's session. Never combine it
   with `allow-scripts`, which would let a mail's script take the page's place.
2. **A Content-Security-Policy** in the frame's own document: images from your app (`'self'`) and
   `data:` only, styles inline only, no fonts, scripts, frames or connections. That blocks
   remote images in CSS `url()`s too, which no markup rewrite finds.
3. **A rewrite of the markup** before it is shown: scripts, frames, `<base>`, `<meta>` and
   `<link>` removed, `on…` attributes dropped, remote `src`s set aside (and counted, for a "Show
   images" button), links opened in a new tab, and `cid:` references rewritten to the inline
   images' download URLs.

**`cid:` images.** An inline image is a `mail_attachments` row with `inline = true` and its
Content-ID in `cid`, and `body_html` refers to it as `<img src="cid:…">` (the module does not
embed it). Its bytes are at the attachment's File field, `GET /api/mail_attachments/{id}/file`.
In ingest mode the file is stored on import; in mirror mode it is stored when first opened, so
call `mail_attachment` first when `file` is still empty.

```ts
// src/mail/mailHtml.ts
/** The inline images of one message: Content-ID (no angle brackets) → the URL of its file. */
export type InlineImages = Record<string, string>;

/** Elements a mail never needs, which could navigate, run, frame or restyle the page. */
const DROP = "script, noscript, iframe, frame, frameset, object, embed, applet, base, link, meta";
/** Attributes that load something. */
const LOADS = ["src", "background", "poster"];
const REMOTE = /^(https?:)?\/\//i;

function cidOf(url: string): string {
  const raw = url.trim().slice(4).replace(/^<|>$/g, "");
  try {
    return decodeURIComponent(raw);
  } catch {
    return raw;
  }
}

/**
 * A mail's HTML as an iframe's `srcdoc`: rewritten as above, with a Content-Security-Policy that
 * allows images from this app and `data:` only (and remote ones when `showRemote`). `blocked`
 * counts the remote images set aside, for a "Show images" button.
 */
export function mailDocument(html: string, images: InlineImages, showRemote = false): { srcdoc: string; blocked: number } {
  const doc = new DOMParser().parseFromString(html, "text/html");
  doc.querySelectorAll(DROP).forEach((el) => el.remove());
  let blocked = 0;
  for (const el of Array.from(doc.querySelectorAll("*"))) {
    for (const attr of Array.from(el.attributes)) {
      if (attr.name.toLowerCase().startsWith("on")) el.removeAttribute(attr.name);
    }
    for (const name of LOADS) {
      const value = el.getAttribute(name);
      if (value === null) continue;
      if (/^\s*cid:/i.test(value)) {
        const cid = cidOf(value);
        const url = images[cid] ?? images[value.trim().slice(4)];
        if (url) el.setAttribute(name, url);
        else el.removeAttribute(name);
      } else if (REMOTE.test(value.trim()) && !showRemote) {
        el.setAttribute(`data-blocked-${name}`, value);
        el.removeAttribute(name);
        blocked++;
      }
    }
    if (el.hasAttribute("srcset") && !showRemote) el.removeAttribute("srcset");
    if (el.tagName === "A" && el.hasAttribute("href")) {
      if (/^\s*(javascript|data|vbscript):/i.test(el.getAttribute("href") ?? "")) el.removeAttribute("href");
      el.setAttribute("target", "_blank");
      el.setAttribute("rel", "noopener noreferrer");
    }
  }
  const remote = showRemote ? " https: http:" : "";
  const csp = doc.createElement("meta");
  csp.setAttribute("http-equiv", "Content-Security-Policy");
  csp.setAttribute(
    "content",
    `default-src 'none'; img-src 'self' data:${remote}; style-src 'unsafe-inline'${remote}; font-src data:${remote}`,
  );
  doc.head.prepend(csp);
  return { srcdoc: `<!doctype html>${doc.documentElement.outerHTML}`, blocked };
}
```

The parse is `DOMParser`'s, which builds an inert document: nothing in it loads or runs while it
is rewritten. The policy is the frame's first element, so it covers everything after it, and a
policy of the mail's own could only make it stricter.

```tsx
// src/mail/Message.tsx
import { useEffect, useMemo, useRef, useState } from "react";
import { useLiveRows } from "../feldspar/live-react";
import { api } from "../feldspar/hooks";
import { mailDocument, type InlineImages } from "./mailHtml";

const fileUrl = (attachmentId: number) => `/api/mail_attachments/${attachmentId}/file`;

export function Message({ id }: { id: number }) {
  const [message] = useLiveRows(api.mail_messages, { where: { id: `eq.${id}` } }).rows;
  const attachments = useLiveRows(api.mail_attachments, { where: { message: `eq.${id}` } }).rows;
  const [showRemote, setShowRemote] = useState(false);
  const [images, setImages] = useState<InlineImages>({});
  const frame = useRef<HTMLIFrameElement>(null);

  // Older mirror mail is stored as headers only (body_days): fetch it whole when opened.
  useEffect(() => {
    if (message?.body_state === "headers") void api.runMailFetchBody({ message_id: id });
  }, [id, message?.body_state]);

  // Opening a message marks it read, on the server too.
  useEffect(() => {
    if (message && !message.seen) void api.runMailSetFlags({ message_ids: [id], seen: true });
  }, [id, message?.seen]);

  // The inline images, each stored first if this is its first opening.
  const inline = attachments.filter((a) => a.inline && a.cid);
  const inlineKey = inline.map((a) => `${a.id}:${a.file ?? ""}`).join(",");
  useEffect(() => {
    let cancelled = false;
    Promise.all(
      inline.map(async (a) => {
        if (!a.file) await api.runMailAttachment({ attachment_id: a.id });
        return [a.cid as string, fileUrl(a.id)] as const;
      }),
    ).then((pairs) => !cancelled && setImages(Object.fromEntries(pairs)));
    return () => {
      cancelled = true;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [inlineKey]);

  const shown = useMemo(
    () => (message?.body_html ? mailDocument(message.body_html, images, showRemote) : null),
    [message?.body_html, images, showRemote],
  );
  if (!message) return null;

  const download = async (attachmentId: number) => {
    await api.runMailAttachment({ attachment_id: attachmentId }); // stores it if it is not yet
    window.open(fileUrl(attachmentId), "_blank", "noopener");
  };

  return (
    <article>
      <h2>{message.subject}</h2>
      <p>{message.from_name} &lt;{message.from_addr}&gt;</p>
      {shown && shown.blocked > 0 && !showRemote && (
        <button onClick={() => setShowRemote(true)}>Show {shown.blocked} remote images</button>
      )}
      {shown ? (
        <iframe
          ref={frame}
          title={message.subject ?? "message"}
          sandbox="allow-same-origin allow-popups allow-popups-to-escape-sandbox"
          referrerPolicy="no-referrer"
          srcDoc={shown.srcdoc}
          style={{ width: "100%", border: 0 }}
          onLoad={() => {
            const f = frame.current;
            if (f?.contentDocument) f.style.height = `${f.contentDocument.documentElement.scrollHeight}px`;
          }}
        />
      ) : (
        // A text-only mail: React escapes it, and pre-wrap keeps its lines.
        <pre style={{ whiteSpace: "pre-wrap" }}>{message.body_text}</pre>
      )}
      <ul>
        {attachments.filter((a) => !a.inline).map((a) => (
          <li key={a.id}>
            <button onClick={() => download(a.id)}>{a.filename}</button> ({Math.ceil((a.size ?? 0) / 1024)} kB)
          </li>
        ))}
      </ul>
    </article>
  );
}
```

`body_text` is always there for a message with a body, including the module's placeholders for
mail with none ("this message is encrypted", TODO-imap.md §5.4), so the `<pre>` is also the fallback while
a `headers` message is being fetched. When it arrives, the live row updates and the frame
appears. Treat `subject`, `from_name` and the file names as text, as above: never as HTML.

### Reply

A reply is `mail_send` (Step 8) with `in_reply_to`, so the threading headers are the module's
job, and the sent copy lands in Sent and in the thread (`thread_key`) as a row:

```tsx
// src/mail/Reply.tsx
import { useState } from "react";
import { api } from "../feldspar/hooks";

type Original = { id: number; account: number; subject: string | null; from_addr: string | null; reply_to: unknown };

export function Reply({ original, done }: { original: Original; done: () => void }) {
  const replyTo = Array.isArray(original.reply_to) && original.reply_to[0]?.address;
  const [to, setTo] = useState<string>(replyTo || original.from_addr || "");
  const [subject, setSubject] = useState(/^re:/i.test(original.subject ?? "") ? original.subject! : `Re: ${original.subject ?? ""}`);
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);

  const send = async () => {
    try {
      const sent = (await api.runMailSend({
        account_id: original.account,
        to,
        subject,
        text,
        in_reply_to: original.id,
      })) as { saved: boolean; save_error: string | null };
      if (!sent.saved && sent.save_error) console.warn(`sent, but not saved in Sent: ${sent.save_error}`);
      done();
    } catch (e) {
      setError(String(e)); // a bad address, too many recipients, a refused login: nothing was sent
    }
  };

  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        void send();
      }}
    >
      <input value={to} onChange={(e) => setTo(e.target.value)} />
      <input value={subject} onChange={(e) => setSubject(e.target.value)} />
      <textarea value={text} onChange={(e) => setText(e.target.value)} />
      {error && <p role="alert">{error}</p>}
      <button>Send</button>
    </form>
  );
}
```

**Attachments** are files of your application's own, read as the sender (Step 8): give the app a
table with a File field (say `outgoing`, with `file` in the store `uploads`, folder `outgoing`),
upload into it with `api.outgoing.file.upload(row.id, f.name, f)`, and pass
`attachments: [{ store: "uploads", path: row.file }]`. To forward a mail's own attachment, pass
`{ attachment_id }`. A **draft** is `mail_save_draft` with the same payload. Saving again with
`replaces: <the previous draft's id>` keeps one draft, and `draft_id` on the send deletes it.

**What this step does not need.** No polling (the rows are live), no IMAP in the browser, and no
password in the app: the client only ever sees `••••••••` in `imap_password`. Writing a field
the service owns (`status`, `seen`, the folder state) through REST is overwritten at the next
sync. Change flags, folders and mail through the actions, which do it on the server first.
