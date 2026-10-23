# @feldspar/imap: mail for application authors

What an application built on the bundled mail module needs to know, on one page. The full
walk-through is [docs/tutorial-mail.md](../../docs/tutorial-mail.md); the design is
[TODO-imap.md](../../TODO-imap.md).

## Two modes

Each `mail_accounts` row has a `mode`:

| | `ingest` | `mirror` |
|---|---|---|
| for | a shared mailbox the app processes (orders@, support@) | a person's mail client |
| folders | the ones in `folders` (default `["INBOX"]`) | every folder on the server |
| rows | never deleted: an expunge clears `uid`, a move updates `folder` | follow the server: deleted mail is deleted, a new UIDVALIDITY re-syncs the folder |
| bodies | every mail whole | whole for `body_days` (90); older mail `body_state = "headers"` until `mail_fetch_body` |
| attachments | stored on import | stored when first opened (`mail_attachment`) |
| order | oldest first | newest 50 first, then a backfill of 50 every 15 s |

A key from your own table to `mail_messages` is safe on an ingest account. On a mirror account
the row goes when the mail goes. Don't refer to mirror rows from data you keep, and don't add
foreign keys to them: the module's deletes would be refused, and the row would stay with its `uid`
cleared.

## Who may do what

The four tables carry the module's ownership rule. REST, GraphQL, live streams and
`db.asUser()` all apply it:

- **A personal account** (`owner` set) and its folders, messages and attachments: its owner
  only. The owner may insert the account (with `owner` = their own id), read it (passwords come
  back as `••••••••`), and update or delete it, through `PUT /api/mail_accounts/<id>` and the
  rest. Send `••••••••` back unchanged to keep a password.
- **A shared account** (`owner` empty): readable by users at or above `min_role`, changed only by
  an admin. Its users may send from it at or above `send_role` (an admin only when empty).
- **Messages and folders**: whoever may read them may also set flags on them, move them, delete
  them and file them, through the actions below. Write the server-side state through the
  actions. A direct update of `seen` changes only the row: the server keeps its own flag, and
  a later sync may overwrite the row with it.

The service writes as the module, not as any user.

## Calling the actions

Make one trigger per action: event **None**, the module's action, minimum role user, named like
the action. Tick it in the application, and build. The generated client then has
`api.runMailSend(…)`, `api.runMailSetFlags(…)` and so on, typed from the module's declarations.
Without the generated client, it is `POST {api mount}/actions/<trigger name>` with the payload as
the body. Each action reads what it touches **as the caller**, so another user's message is
"not found".

| action | payload | modes |
|---|---|---|
| `mail_test_account` | the connection fields, or `{ account_id }` | both |
| `mail_sync_now` | `{ account_id, wait }`: lists the folders again, then syncs | both |
| `mail_set_flags` | `{ message_ids, seen, flagged }` | both |
| `mail_attachment` | `{ attachment_id }` → `{ file }` | both |
| `mail_move` | `{ message_ids, target }`, target a path or `role:trash`, `role:archive`, … | both |
| `mail_fetch_body` | `{ message_id }` | mirror |
| `mail_delete` | `{ message_ids, permanent }` | mirror |
| `mail_create_folder` | `{ account_id, name, parent }` → `{ id, path, role }` | mirror |
| `mail_delete_folder` | `{ folder_id }` or `{ account_id, path }` | mirror |
| `mail_save_draft` | as `mail_send`, plus `replaces` | mirror |
| `mail_send` | `{ account_id, to, cc, bcc, subject, text, html, attachments, in_reply_to, draft_id, calendar }` | both |
| `mail_oauth_start` | `{ account_id, provider, return_to }` → `{ url, provider }`: send the browser to `url` (below) | both |

An action's change is on the server and in the rows when it answers. A live list therefore
updates without a sync.

## Outlook.com, Microsoft 365 and Gmail

`outlook.office365.com` refuses passwords (`LOGINDISABLED`, `AUTH=XOAUTH2` only). Such an
account logs in with a token: call `mail_oauth_start` as the account's owner (an admin, for a
shared account), then `window.location.assign(url)`. The user consents at Microsoft or Google,
and the browser comes back to `return_to`, with `oauth_error=…` in the query if it failed. The
row then has `auth = "oauth2"`, `oauth_provider`, the tokens in the Secret fields
`oauth_access_token` and `oauth_refresh_token`, and `oauth_expires_at`; empty IMAP and SMTP
servers are filled with the provider's. The module renews the token itself. When the provider
refuses to renew it, the account goes to `auth_failed`, and connecting it again is
`mail_oauth_start` once more. `provider` is `microsoft` or `google`, found from the address when
left out. Tutorial Step 11 has the admin's half.

## Columns a client uses

- `mail_messages`: `subject`, `from_name`/`from_addr`, `to_addrs`/`cc_addrs`/`reply_to` (lists
  of `{ name, address }`), `sent_at` (the Date header) and `internal_date` (received; both
  indexed with `folder`), `snippet`, `body_text`, `body_html` (render it sanitised: tutorial
  Step 10), `seen`/`flagged`/`answered`/`draft`, `keywords` (`$label1` … and other keywords,
  sorted), `headers` (below), `thread_key` (indexed; group conversations by it),
  `body_state`, `import_error`.
- `mail_folders`: `path`, `name`, `role` (`inbox`, `sent`, `drafts`, `trash`, `spam`,
  `archive`), `unread`, `total`, `backfill_uid` (0 when the folder is complete).
- `mail_accounts`: `status` (`connecting`, `syncing`, `idle`, `error`, `auth_failed`,
  `refused`, `disabled`), `last_error`, `last_sync_at`, `auth` (`password` or `oauth2`),
  `oauth_provider`.

## Settings (admin)

**Settings → Modules → Mail → Settings**, or the MCP tool `updateModule` (modules area). A save
reloads the module and restarts its service. Writing `_fd_modules` directly does neither.

- **Trusted servers** and **Extra CA certificates**: mail servers on a private network.
- **Certificate names** (`host name` per line): check the certificate of a server dialled by IP
  against the name it is issued for. **Servers whose certificate is not checked** (`host` or
  `host:port` per line): accept any certificate from them, e.g. a self-signed one.
- **Sign-in providers**: the Microsoft and Google apps OAuth2 accounts use (client id and secret,
  and Microsoft's tenant), each with the redirect address
  `https://<admin host>/_feldspar/oauth/callback`. Tutorial Step 11 says how to register them.
- **Headers to keep**: the header fields stored in `mail_messages.headers`, by lower-case name,
  one per line, `x-spam-*` for a prefix. Empty means `list-id`, `list-unsubscribe`,
  `list-unsubscribe-post`, `authentication-results`, `x-spam-*`, `precedence` and
  `auto-submitted`; `-` means none. It applies to mail stored from then on.
