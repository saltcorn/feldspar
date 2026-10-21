# Saltcorn v2 — Mail: the bundled IMAP/SMTP module (milestones M1–M10)

Ordered, checkable task list for **reading and sending email from applications**: a bundled
module, `@feldspar/imap`, that keeps IMAP mailboxes synced into ordinary tables in near real
time, plus the core changes it needs. It replaces two earlier attempts:

- the Saltcorn v1 module [saltcorn/imap](https://github.com/saltcorn/imap). It is an
  `imap_sync` action that runs every 5 minutes and asks the admin to map each mail field onto a
  column of their own table. Its **protocol handling is battle-tested** across many servers,
  and its lessons are listed in §6;
- `feldspar-mailbridge`, a Python module of stateless `modfn.mail_*` functions. It was written in
  an afternoon and tested against one account. It is **design input only**, not proven code (§7).

This builds on modules (`plugins/README.md`, [docs/tutorial-modules.md](./docs/tutorial-modules.md),
TECHNICAL_DESIGN §11), ownership ([docs/tutorial-ownership.md](./docs/tutorial-ownership.md)),
triggers and actions ([docs/tutorial-triggers.md](./docs/tutorial-triggers.md)), live updates
([TODO-live-updates.md](./TODO-live-updates.md), `useLiveRows`), file fields
([docs/tutorial-file-fields.md](./docs/tutorial-file-fields.md)), `send_email`
(`crates/sc-core-actions/src/send_email.rs`) and backup (`crates/sc-server/src/backup/`).

The design comes first, then the milestones. Each milestone is prefixed **M** and its tasks are
numbered M1.1, M1.2, …. Milestones M2–M6 (core) and M7 (parsing) do not depend on each other and
can be done in any order. M8 needs M2, M4, M5 and M7. M9 needs M8 and M6.

Legend: `[ ]` todo · `[~]` in progress · `[x]` done.

---

# Part I — Design

## 1. What has to work

| | Use case | Accounts | Who sees the mail | Shape |
|---|---|---|---|---|
| **A** | **Shared ingest mailbox.** Orders are mailed to orders@, and every staff user sees them. The commonest case in the applications built so far. | one, or a few, entered by the **admin** | every user with a minimum role | append-only; full bodies and attachments stored straight away; an Insert trigger turns a mail into an order |
| **B** | **Personal mail client.** Each user enters IMAP and SMTP credentials for **several** accounts of their own, then reads and sends mail in a React app. | any number, entered by **each user** | the owner only | a two-way mirror: flags, moves and deletes go back to the server |
| **C** | **Automated sending from an account** (e.g. "your order has shipped", from orders@) | either kind | – | the built-in `send_email` action, through that account's SMTP server |
| **D** | **Compose and send from the client** (B), with reply threading and a copy in Sent | B's | – | a module action called from the app |

New mail must appear **within seconds**, using IMAP IDLE, not every 5 minutes. The admin should
**map no fields**: the module owns fixed tables, and an application's own data refers to them.

## 2. Decisions taken

1. **JavaScript**, with [imapflow](https://github.com/postalsys/imapflow) (≥ 1.0.195, the version
   saltcorn/imap pinned last), [mailparser](https://github.com/nodemailer/mailparser) and
   [nodemailer](https://github.com/nodemailer/nodemailer). These were chosen over Python for
   the following reasons:
   - Python's `imaplib` has no IDLE before Python 3.14 (Debian 13 has 3.13), returns raw byte
     responses, and its LOGIN is ASCII-only.
   - imapflow has IDLE, CONDSTORE/QRESYNC, XOAUTH2 and a real response parser.
   - The JS module works on the static build, and it gets the `net` permission sandbox.
   - Python's `email` package is better than mailparser at broken MIME. The corpus tests (M1, M7)
     are what close that gap.
2. **The module owns its tables** (§3), declared by the module and created by the host (M4).
3. **A background service** holds the IDLE connections and writes rows (M5). Today a module can
   only reach `db` from inside a call that has a caller (`module-host.mjs` `ask`/`noCaller`).
4. **A new `Secret` field kind** stores passwords encrypted at rest (M2):
   - REST, GraphQL and live updates never return them;
   - server-side code can read them: action templates, code bodies, module actions and services;
   - **backups include them, and enough to decrypt them**, so a restore after a disaster needs
     no passwords re-entered.
5. **No transport facility.** `send_email` gets optional SMTP override settings instead (M3).
6. **No frontend scaffolding.** The module supplies tables, a service and actions. The mail client
   is the application builder's own work.
7. **Shared ingest (A) is built first** (M8). Mirror (B) and its actions follow (M9).

## 3. Tables

The module declares them (M4), so the names below are its own. All of them use an `id` primary
key, matching the module tables convention chosen in M4.

**`mail_accounts`**

| field | type | notes |
|---|---|---|
| `owner` | Key → users, nullable | empty means a **shared** account |
| `min_role` | Int (role id) | shared accounts: the least role that can read its mail |
| `send_role` | Int (role id) | shared accounts: the least role that can send from it |
| `label`, `email`, `display_name` | Text | |
| `mode` | Text: `ingest` \| `mirror` | §4 |
| `imap_host`, `imap_port`, `imap_security`, `imap_user` | Text / Int | security: `tls` (993) or `starttls` (143) |
| `imap_password` | **Secret** | |
| `smtp_host`, `smtp_port`, `smtp_security`, `smtp_user` | Text / Int | 465 = tls, 587 = starttls |
| `smtp_password` | **Secret** | |
| `auth` | Text: `password` (later `oauth2`) | |
| `folders` | Json | ingest: which folders to watch (default `["INBOX"]`) |
| `processed_folder`, `error_folder` | Text, nullable | ingest: move a mail there after import, or after a failed import |
| `start_from` | Timestamp, nullable | ingest: import mail on or after this date only; empty means everything |
| `body_days` | Int (default 90) | mirror: full bodies for this many days, headers only for older mail |
| `enabled` | Bool | |
| `status`, `last_error`, `last_sync_at` | Text / Text / Timestamp | written by the service only |

**`mail_folders`**: `account`, `path`, `name`, `role` (`inbox` `sent` `drafts` `trash` `spam`
`archive` or empty), `delimiter`, `uidvalidity`, `last_uid`, `highestmodseq`, `unread`, `total`,
`sync` (Bool). Unique on `(account, path)`.

**`mail_messages`**:
- links: `account` (kept here as well, for the ownership rule), `folder`, `uid`;
- headers: `message_id`, `in_reply_to`, `references`, `thread_key`, `subject`, `from_name`,
  `from_addr`, `to_addrs`, `cc_addrs`, `reply_to`, `sent_at`, `internal_date`;
- content: `snippet`, `body_text`, `body_html`, `size`, `has_attachments`;
- flags: `seen`, `flagged`, `answered`, `draft`;
- `body_state`: `full`, `headers` or `too_large`;
- `import_error` (Text, nullable).

It is unique on `(folder, uid)`. When an ingest account moves a mail to `processed_folder`, the row
stays and records where the mail went.

**`mail_attachments`**: `message`, `part` (the IMAP part path, e.g. `2.1`), `filename`,
`content_type`, `size`, `cid`, `inline` (Bool), `file` (a File field in the module's store, cached
the first time it is opened, or straight away in ingest mode).

**Ownership** (`docs/tutorial-ownership.md`), on every table except `mail_accounts`:

```
accountⱵowner === user.id || (accountⱵowner === null && user.role <= accountⱵmin_role)
```

(`mail_attachments` reaches the account through `messageⱵaccount`.) On `mail_accounts` itself:
- **read:** the same rule, minus the join;
- **insert and update:** only rows with `owner === user.id`, or by an admin. That stops a
  non-admin making a shared account or taking over someone else's. As one formula (M4.4):
  `owner === user.id || (_read && owner === null && user.role <= min_role)`.

Writes to `status`, `last_error`, `last_sync_at`, the folder state and every message flag go
through the service or the module's actions, not REST. The ownership model cannot make some
fields read-only for owners (M4.4: a formula grants rows, not fields), so the actions re-check,
and the service overwrites any client write to those fields on the next sync.

**An application's own data refers to these tables.** Mail is not copied into it. For example,
`orders.message` is a Key → `mail_messages`, filled by an **Insert trigger on `mail_messages`**
filtered to one account. Its code body reads `row.subject`, `row.body_text` and the attachment
rows. M4 must keep fields an admin adds to a module table (e.g. a `handled` flag on
`mail_messages`) through module upgrades.

## 4. Two account modes

| | `ingest` (A) | `mirror` (B) |
|---|---|---|
| mail gone from the server | **the row stays** | the row is deleted |
| flags changed on the server | updated | updated |
| flags changed in the app | updated on the server (`mail_set_flags`) | updated on the server |
| bodies | always full (up to the size cap, §5.4) | full for `body_days`, headers only for older mail, fetched when opened |
| attachments | stored to the file store on import | stored when first opened |
| folders | the `folders` list | all of them. INBOX uses IDLE, the others are polled |
| after import | optional move to `processed_folder` (or to `error_folder` on failure) | – |
| first sync | from `start_from`, oldest first | newest first, then slow backfill |
| actions allowed | test, sync now, attachment, set flags, move, send (with `send_role`) | all (§5.6) |

## 5. The sync service

### 5.1 Connections
- **Each enabled account:**
  - **one IDLE connection** on INBOX (on an ingest account, on its first watched folder);
  - **one short-lived connection** for everything else, opened when needed and closed after 60 s idle.
  - That stays below per-user connection limits (Gmail allows 15).
- **imapflow does not reconnect by itself.** On `close` or `error`, reconnect with exponential
  backoff and jitter, from 5 s up to 10 min.
- **Login failures:** after 3 in a row, set `status = auth_failed` and stop retrying until the row
  changes. Repeated bad logins can get an account locked.
- **IDLE is restarted every 25 minutes.** imapflow does this, but check it.
- **Connection rules,** used by every connection the module opens and by M3:
  - **ports:** IMAP 993 or 143 with STARTTLS required; SMTP 465 or 587;
  - **public addresses only:** the host must resolve to globally routable addresses only, and the
    connection goes to the address that was checked (no second lookup);
  - certificates are always verified;
  - imapflow's own timeouts are set, and the logger stays off unless debugging;
  - passwords never appear in a log, an error or `last_error`.

### 5.2 Folder state and new mail
These are the lessons from saltcorn/imap, kept:
- **UIDs only**, never sequence numbers, for fetch, download, move and flags.
- **`uidvalidity` per folder.** When it changes, the folder's rows are reconciled by `message_id`
  (mirror: rebuilt; ingest: matched where possible, never deleted). The cursor is then reset.
- **The cursor is `mail_folders.last_uid`**, not `max(uid)` of the messages table. Fetch
  `last_uid+1:*` and **keep only `uid > last_uid`**, because `N:*` always returns the highest
  message even when N is above it.
- **The cursor moves only past messages that were stored.** A message that fails gets a row with
  `import_error` set, so it is visible and can be retried. It does not block the cursor.
- **Each message gets its own try.** One bad mail never stops a batch.
- **Moves and expunges** (`processed_folder`, `error_folder`) happen **after the batch**, by UID.
- **The mailbox lock is released in `finally`.** Logout is wrapped in its own try.
- **Batches:** at most 50 messages per batch. This was unbounded in saltcorn, which had no memory
  protection. The service loops until it catches up.

### 5.3 Flags and expunges
- **When the server has CONDSTORE/QRESYNC:** `CHANGEDSINCE highestmodseq` for flags, and
  `VANISHED` for expunges.
- **Otherwise:** every N minutes, re-read the flags of the newest 2,000 UIDs, and compare
  `UID SEARCH ALL` against the stored UIDs to find expunges. In ingest mode, an expunge is only
  recorded, never deleted.

### 5.4 Parsing (the module's `parse.js`, M7)
**Size cap:** 25 MB.
- **At or below the cap:** fetch the whole source and parse it with mailparser.
- **Above the cap:** keep saltcorn's approach.
  - Fetch `BODYSTRUCTURE` and the headers.
  - `download()` the text parts only.
  - List the attachments from the structure, to be fetched with `download()` when they are opened.

Rules a parsed message must obey:
- **Every header is optional:** no `from`, `to`, `cc` or `date`, or an invalid date, must not throw.
  - `sent_at` falls back to INTERNALDATE.
  - `subject` falls back to an empty string.
- **Charsets:** every text part is decoded by its declared charset. An unknown charset falls back
  to UTF-8, then latin-1, and never throws. Undeclared 8-bit headers are decoded the same way.
- **Body choice:**
  - Keep both `text/html` and `text/plain` when present.
  - HTML only: derive `body_text` from it.
  - Text only: `body_html` stays empty (the client renders the text).
  - A single-part `text/html` mail keeps its markup. (saltcorn/imap lost it.)
- **When there is no body part at all,** `body_text` explains why:
  - TNEF body: use it;
  - encrypted (`multipart/encrypted` or pkcs7): "this message is encrypted";
  - `text/calendar` only: "calendar invitation";
  - attachments only: "this message has only attachments".
- **message/rfc822:**
  - **attached** (disposition `attachment`): stored as an `.eml` attachment. Its body never
    replaces the outer one.
  - **inline** (e.g. Exchange journal reports): recursed into.
- **Attachments:**
  - disposition `attachment`, or a part with a filename and no Content-ID;
  - the name is `filename`, then `name`, then `part-<path>.<ext from type>`;
  - a part with no name and no type is skipped;
  - downloads are binary-safe, never round-tripped through strings, with a configurable chunk
    size (`attachment_chunk_size`, from saltcorn).
- **Inline images:** parts with a Content-ID become attachment rows with `inline = true`.
  `body_html` keeps its `cid:` references, and the client rewrites them to the files' download
  URLs. Nothing is embedded as base64, which saltcorn did with a broken first-match-only replace.
- **TNEF** (`application/ms-tnef`, `winmail.dat`): unpack its body (RTF decompressed to text)
  and its attachments, in JS (M7.4). No external binary and no temporary files.
- **Remove NUL bytes** from every text value. Postgres refuses them.
- **Size limits:** `body_text` and `body_html` at most 400,000 characters each, and `snippet` 200,
  with the rest of the header lengths taken from the mailbridge's `_parse`.

### 5.5 How the service learns about account changes
A user saves an account; the service has to start, restart or stop its connections.
- **Preferred:** M5's `ctx.onTableChange("mail_accounts", …)`, if it is cheap to provide through
  the live hub.
- **Otherwise:** re-read `mail_accounts` every 15 s, comparing an `updated_at` or a hash of the
  connection fields.

### 5.6 Actions
- **Exposure:** each action is exposed as a trigger with event `None` and `min_role` user, and
  ticked in the application.
- **Authorization:** each action loads the account **as the caller** (`asUser()`), so the
  ownership rule decides. Only then does it read the secrets. Sending from a shared account also
  needs `user.role <= send_role`.

| action | payload | does |
|---|---|---|
| `mail_test_account` | the connection fields (not yet saved) | logs in to IMAP and SMTP, returns `{imap, smtp, error}`. Rate-limited per user |
| `mail_sync_now` | `account_id` | wakes the service for that account |
| `mail_fetch_body` | `message_id` (row) | fetches a body that is still headers-only, updates the row |
| `mail_attachment` | `attachment_id` | stores the file if it isn't stored yet. Returns the File path for the normal download |
| `mail_set_flags` | `message_ids`, `seen`, `flagged` | on the server, then on the rows |
| `mail_move` | `message_ids`, `target` (path or `role:trash` …) | `MOVE` by UID, then the rows |
| `mail_delete` | `message_ids`, `permanent` | to Trash, or `\Deleted` and expunge |
| `mail_create_folder` | `account_id`, `name`, `parent` | mirror only |
| `mail_save_draft` | the message, `replaces` | `APPEND` to Drafts with `\Draft` (and deletes the draft it replaces) |
| `mail_send` | `account_id`, `to`, `cc`, `bcc`, `subject`, `text`, `html`, `attachments` (File paths), `in_reply_to`, `draft_id` | §5.7 |

### 5.7 Sending (`mail_send`, use case D)
1. Load the account as the caller and check `send_role`. Read `smtp_password`.
2. Check the input:
   - every address is valid; at most 50 recipients;
   - attachments are File paths the caller can read: at most 10 files and 20 MB in total.
3. Build the message with nodemailer:
   - From is `display_name <email>`;
   - Message-ID is on the account's domain;
   - for a reply, `In-Reply-To` and `References` are taken from the message being answered.
4. Send through SMTP, using §5.1's connection rules. nodemailer sends UTF-8 credentials with AUTH
   PLAIN; test this with a password containing umlauts.
5. `APPEND` the copy to the folder with role `sent` (created if it is missing), with `\Seen`. Some
   servers file SMTP-sent mail into Sent themselves (Gmail, Microsoft 365), so skip the APPEND
   when a setting says so, or when the service sees the copy arrive.
6. Insert the copy's row. Delete the draft `draft_id`.
7. Return `{message_id, saved, save_error}`. A failed APPEND does not fail the send.

## 6. Lessons from saltcorn/imap that M7–M9 must keep

1. UIDs everywhere, never sequence numbers (e873918, d14e402).
2. Move only after the batch (e616885).
3. `N:*` returns the last message even when N is past it: filter `uid > last` (05650c2).
4. Download attachments binary-safe, with a configurable chunk size (c88f676, cef2546).
5. `dispositionParameters` and `parameters` may be missing. Skip parts with no name and no type
   (f422f71).
6. No `req` in the scheduler. Crash logging must not need one (7907683, 56ce6d2).
7. Decode quoted-printable and base64, then decode **by charset**, including base64 bodies,
   which saltcorn did not (cdee382, e873918, f0a901b).
8. Fall back to the plain text when there is no HTML part (47b7290).
9. Single-part mail needs full-source parsing (4209b07).
10. Embed inline images only when they have a Content-ID (0b77ea9, c2b135e).
11. Missing `to` (ce26afb), and likewise `cc`, `from` and `date`.
12. Attached .eml: save as a file. Inline rfc822: recurse into it (63f0561, 6d7ea45, 3087681).
13. Unpack TNEF (78294d1).
14. Errors are handled per message; record a crash; error folder; error trigger (82f1674,
    a9f4196, ef01b74, b5b76f3). Here, the error trigger is simply an Insert or Update trigger on
    `mail_messages` where `import_error` is set.
15. Release the mailbox lock in `finally`, and wrap logout in try.
16. imapflow's logger off by default (d8f54a6).
17. OAuth2 / XOAUTH2 is needed for Gmail and Microsoft 365. Microsoft is retiring basic
    authentication, so this is M10.3, not "never".

## 7. Ideas from feldspar-mailbridge (untested; use them only with a test of your own)

These are in `~/feldspar-mailbridge/feldspar_mailbridge/core.py` on the original dev host:

- `_resolve`: allowed ports, public addresses only, connecting to the checked IP (§5.1).
- `_login_imap` / `_login_smtp`: AUTH PLAIN for UTF-8 credentials.
- `_parse`: the placeholder texts for mail with no body (§5.4), the size limits, `_no_nul`.
- `_fix8`: decoding undeclared 8-bit headers.
- `_special_folder`: finding the Sent, Drafts, Trash, Archive and Spam folders by SPECIAL-USE
  flag, then by common names, in the server's namespace.
- `tnef.py`: a TNEF and compressed-RTF decoder in about 300 lines, as a reference for M7.4.
- `risk.py`: phishing-risk scoring (lookalike domains, link text versus link target). Later (M10).
- `tests/edge_lib.py`: generators for awkward MIME (TNEF, RTF), for the corpus (M1.2).

---

# Part II — Milestones

Working mode is as in CLAUDE.md: a test for every task, a CHANGELOG entry, mark the task done,
and do not commit.

## M1 — Test corpus and the spike

- [x] **M1.1** `plugins/imap/test/fetch-corpus.sh`. It shallow-clones these into
  `plugins/imap/test/corpus/<source>/`, which is git-ignored, and records the commit hashes in
  `corpus.lock`:
  - [stalwartlabs/mail-parser](https://github.com/stalwartlabs/mail-parser), Apache-2/MIT:
    `resources/eml/{regressions,malformed,legacy,thirdparty,rfc,issues}`. About 200 `.eml`, each
    with an expected-output `.json`;
  - [zbateson/mail-mime-parser](https://github.com/zbateson/mail-mime-parser), BSD-2:
    `tests/_data/emails` (78);
  - [mikel/mail](https://github.com/mikel/mail), MIT: `spec/fixtures/emails`, about 107,
    including `error_emails`;
  - [apache/james-mime4j](https://github.com/apache/james-mime4j), Apache-2:
    `core/src/test/resources/{testmsgs,mimetools-testmsgs}` (80);
  - [python/cpython](https://github.com/python/cpython), PSF: `Lib/test/test_email/data` (65,
    sparse checkout);
  - [php-mime-mail-parser](https://github.com/php-mime-mail-parser/php-mime-mail-parser), MIT:
    `tests/mails` (47);
  - [mailgun/flanker](https://github.com/mailgun/flanker), Apache-2: `tests/fixtures/messages`
    (about 50: bounces, delivery-status reports);
  - [SpamScope/mail-parser](https://github.com/SpamScope/mail-parser), Apache-2: `tests/mails` (23);
  - [nodemailer/mailparser](https://github.com/nodemailer/mailparser), MIT: `test/fixtures` (10);
  - [verdammelt/tnef](https://github.com/verdammelt/tnef), **GPL-2**: `tests/files/datafiles`
    (15 `.tnef`, each with a `.list` of the attachment names it should contain);
  - the [SpamAssassin public corpus](https://spamassassin.apache.org/old/publiccorpus/), about
    6,000 real mails from 2002–2003. The copyright stays with the senders.

  The corpus is fetched, not committed. A permissive subset can be committed later under
  `test/fixtures/` with an `ATTRIBUTION.md`, but never the GPL TNEF files or the SpamAssassin
  mails.
- [x] **M1.2** `plugins/imap/test/fixtures/generated/`: generated mails for every lesson in §6
  and every rule in §5.4 that the public sets don't cover. Examples:
  - base64 body in windows-1252;
  - an unknown charset;
  - single-part HTML;
  - no From and no Date;
  - inline rfc822 next to attached rfc822;
  - cid images with single quotes and with repeated references;
  - a NUL byte in the body;
  - a TNEF mail with RTF only.

  Committed, with the generator script.
- [x] **M1.3** A list of mails to send by hand from the clients the corpus lacks, written to
  `plugins/imap/test/MANUAL-SAMPLES.md`:
  - clients: Outlook desktop and Microsoft 365 (internal to Exchange, so TNEF), Outlook on the
    web, Gmail web, Apple Mail, iOS Mail, Thunderbird;
  - per client: plain, HTML, an inline image, two attachments, a reply, a forward-as-attachment,
    a calendar invite.

  Raw sources go into `test/fixtures/manual/` once someone has sent them.
- [x] **M1.4** **Spike** (throwaway code, in `plugins/imap/spike/`). Install a scratch module
  with `net: ["*"]` and check, inside feldspar's Deno worker:
  - imapflow connects over `node:tls` and logs in with a UTF-8 password;
  - **IDLE stays open across module calls** and receives `exists`;
  - mailparser parses a 20 MB mail within the 256 MB heap;
  - nodemailer sends through 587 and through 465.

  Test against a local Dovecot and a local SMTP sink, in Docker. Write the findings into this
  file under M1.4, including anything Deno's node-compatibility needed. If imapflow or nodemailer
  cannot be made to work, **stop and reconsider §2.1** before going further.

  **Findings (2026-10-21).** imapflow 2.2.6, mailparser 3.9.36 and nodemailer 10.0.15 all work
  inside the Deno worker. §2.1 stands. Everything in the list was checked except Docker, which this
  host does not have. `spike/servers/dovecot.sh` runs Debian's own Dovecot 2.4.1 instead,
  unpacked as an ordinary user; `spike/servers/smtp-sink.mjs` is nodemailer's `smtp-server` on 465
  and 587. `spike/run-spike.sh` runs it all; the harness is
  `crates/sc-module/tests/imap_spike.rs` (`#[ignore]`, and it skips without `IMAP_SPIKE_DIR`).
  Delete both with the spike.

  | claim | result |
  |---|---|
  | imapflow over `node:tls` (993) and STARTTLS (143), UTF-8 password | works: AUTHENTICATE PLAIN (imapflow's default) and LOGIN both log in with `Grüße-€-pässwort`. Login takes 60–160 ms |
  | IDLE stays open across module calls and receives `exists` | works. A mail delivered by `doveadm save` while **no call was running** raised `exists` 530–550 ms after delivery, so the worker's event loop runs between calls. A second delivery during a 20 MB parse arrived too, and the IDLE connection was still `usable` after every other call in the run |
  | mailparser on a 20 MB mail within the 256 MB heap | works: 20 MB parses in 0.55 s and 25 MB (the cap) in 0.69 s, the attachment bytes intact. `heap_size_limit` reads 256 MB inside the worker. The heap seen was 60 → 77 MB for 20 MB and 68 → 88 MB for 25 MB, but that sample cannot run during the synchronous part of the parse. What is proven is that it did not hit the bound. No call came near the 10 s JS slice |
  | nodemailer through 587 and 465 | works: 465 with TLS and AUTH PLAIN, 587 with `requireTLS` and AUTH LOGIN, both with the UTF-8 password, and the sink saw an authenticated, TLS session for each |

  What Deno's node compatibility needed, and other things M7–M9 must know:

  1. **`os.hostname()` was refused, and is now granted.** Feldspar granted a module no `sys`
     at all, and imapflow could not be required: its logger, `pino`, calls `os.hostname()` at
     load time, and the load failed with "denied sys access to \"hostname\"". The host now
     grants every module `sys: hostname` and nothing else of `sys` (`deno/wiring.rs`
     `container`), so `os.hostname()` returns the machine's real name. The spike loads without
     a workaround. nodemailer also uses the hostname as its EHLO name, so `mail_send` and M3's
     override still pass `name: <the account's domain>`: the EHLO name should be the sending
     domain, not the machine.
  2. **Certificate checks work as in Node.** Without the CA, imapflow fails with
     `UNABLE_TO_VERIFY_LEAF_SIGNATURE` and nodemailer with "unable to verify the first
     certificate". Connecting to `127.0.0.1` with `tls.servername: "wrong.example"` fails with
     `ERR_TLS_CERT_ALTNAME_INVALID`, so §5.1's "connect to the checked IP, verify the name" is
     `host: <ip>` plus `tls.servername: <name>`, for both libraries.
  3. **Always attach an `error` listener to an ImapFlow.** Without one, a refused login
     emits `error` after `connect()` has already rejected, and in Node that is an uncaught
     exception that ends the process (seen in the Node pre-check). In the worker it would end
     the worker and every module on it.
  4. **A failed login slows the next ones.** After one wrong password, Dovecot's per-IP auth
     penalty delayed the next *successful* login from the same address by about 4 s, and the
     failure itself took 1.5–2 s. `mail_test_account` and the `auth_failed` rule (§5.1) should
     expect this, and the per-user rate limit on `mail_test_account` matters for that reason too.
  5. imapflow restarts IDLE every 25 minutes by itself (§5.1). That was not checked here; the
     run was 20 s long. M8.7's tests should hold an IDLE for 30 minutes once.
  6. Dovecot 2.4 advertises CONDSTORE, QRESYNC, MOVE, UIDPLUS, SPECIAL-USE and IDLE, so M8.4's
     QRESYNC path can be tested locally. The fallback path needs a server without them.

## M2 — `Secret` field kind (core)

- [x] **M2.1** `DataFieldKind::Secret` (`crates/sc-catalog/src/field.rs`) on a `Text` column.
  - The column holds an envelope: `v1:<nonce>:<ciphertext>`, base64, AES-256-GCM or
    XChaCha20-Poly1305 (whichever crypto crate the workspace already has).
  - **Postgres and SQLite** (`sc-db-postgres`, `sc-db-sqlite`) are the same, since it is text.
  - It cannot be part of a key, an index, a unique constraint, a filter or a sort. Refuse those when
    the field is created.
- [x] **M2.2** The instance key.
  - 32 random bytes in `<data dir>/secrets.key`, mode 0600, created the first time it is needed.
  - Restarting a server whose data dir is missing that file refuses to start if any Secret
    value exists, with a message naming the file.
  - Hosted: the key lives in the instance user's home with the rest of `SC_DATA_DIR`, so the
    host's own backup already includes it. Check `scripts/` in feldspar-hosted.
- [x] **M2.3** The row layer encrypts on write and decrypts on read **for server-side callers
  only**: actions, `{{ }}` templates, code bodies (JS and Python), module actions and services.
  - **REST, GraphQL, `useLiveRows`/live streams and the admin data editor** never return the
    value.
  - Instead they return `null` when there is no value, or the redaction sentinel when there is one.
    Reuse `SECRET_SENTINEL` and the `merge_secrets` behaviour from `sc-types/src/field.rs`, so
    that saving a row that sends the sentinel back keeps the stored value.
  - Writing a new value through REST works: write-only.
  - Tests: one per surface, asserting the plaintext never appears in its output.
- [x] **M2.4** Admin UI and generated client.
  - A password input in the data editor and the table designer.
  - The generated TS type is `string | null` with a doc comment saying it is write-only.
- [x] **M2.5** **Backup and restore** (`crates/sc-server/src/backup/`).
  - Secret columns are written as their envelopes, and the backup gets a `keys/secrets.key`
    entry, flagged in the manifest.
  - On restore, each envelope is decrypted with the backup's key and **re-encrypted with the
    target instance's key**, so restoring into a different instance works and the target's key
    is never replaced.
  - A restore without the key entry, or with a wrong key, says so per table and leaves those
    values null instead of failing the restore.
  - Document in OPERATIONS that a backup holding Secret fields can decrypt them, which applies to
    customer S3 backups on hosted too.
- [x] **M2.6** Docs: a "Secret fields" section in `docs/tutorial-ownership.md`, or a new tutorial,
  and the field-type list.

  **Done (2026-10-21).** Decisions worth knowing for M3–M9:
  - **The row layer reads envelopes, and the plaintext is produced explicitly.** Server-side
    readers reveal (`sc_catalog::secrets::reveal_values` / `reveal_json`): trigger events, so
    `{{ row.smtp_password }}` works; `TableHost`, i.e. code bodies, module actions and services;
    and the row actions. Client surfaces redact. A surface nobody thought of shows ciphertext, not
    plaintext.
  - **A secret is not in the formula shape**, so `{{ accountⱵsmtp_password }}` (M3.4) does not
    validate today. M3.4 has to add a join-aware reveal for templates, or use the code-body route.
    A REST `select` through an embed refuses a secret, and so does a code-host Ⱶ path, which reads
    through the formula shape.
  - **Module services (M5) read through `TableHost`** and get the plaintext. That is what
    `ctx.secrets` needs, so M5 may not need a separate API.
  - **Follow-up:** a workflow run's stored event (`sc-workflow` `StoredEvent::of`) keeps the
    revealed row in `_fd_runs`. Redacting or re-sealing it there needs the catalog, which
    `new_run` has not got.

## M3 — SMTP override settings on `send_email` (core)

- [x] **M3.1** New optional settings in `SendEmail::config_spec`:
  - `smtp_host`, `smtp_port`, `smtp_security` (`tls` or `starttls`), `smtp_user`;
  - `smtp_password`, declared with `.secret()` so the trigger editor redacts it.

  All of them are `{{ }}` templates, parsed and checked like the other settings.
- [x] **M3.2** In `run`:
  - when the host renders non-empty, build `EmailSettings` from the rendered settings, call
    `EmailSettings::check`, and send with a one-off `SmtpMailer::new`;
  - otherwise use `ctx.mailer` as now;
  - `from` defaults to `smtp_user` when an override is used and `from` is empty;
  - errors name the host and never the password.
- [x] **M3.3** The connection rules of §5.1 for the override: ports 465 and 587 only; the host
  must resolve to public addresses only, and the connection goes to the address that was checked.
  - lettre resolves names itself. Either resolve first and connect by IP while keeping the name
    for SNI and the certificate check, or check before and accept the small window. Pick one and
    write it down.
  - Server email settings are left alone: the admin's own SMTP may be on localhost.
- [x] **M3.4** Check whether templates can follow joins. In use case C the password is
  `{{ accountⱵsmtp_password }}` on a trigger whose table refers to `mail_accounts`.
  - If they can't, either make templates able to, or document copying the account id into the
    template scope through a code-body step.
  - Templates must see the decrypted Secret value (M2.3).
- [x] **M3.5** Tests with the recording mailer and a fake SMTP: override used, override absent,
  password redacted in the stored trigger, private address refused.
- [x] **M3.6** Docs: `docs/tutorial-triggers.md` `send_email` section. Note that a copy in Sent
  depends on the server (Gmail and Microsoft 365 file it themselves, others don't).

  **Done (2026-10-21).** Decisions worth knowing for M8–M9:
  - **M3.3: resolve first, connect by IP.** `sc_email::relay_mailer` resolves the host once,
    refuses it unless every address is public (`sc_email::is_public`, now shared with the agent
    fetch guard), and builds the transport with `SmtpMailer::pinned`: lettre's
    `builder_dangerous(<ip>)` plus `TlsParameters::new(<host>)`, so SNI and the certificate check
    still use the name and the certificate is always verified. The EHLO name is the sender's
    domain. Ports are 465 (`tls`) and 587 (`starttls`), and a mismatch or `none` is refused.
  - **The seam is `Mailer::send_via(server, email)`.** Its default is the real, checked path,
    and `RecordingMailer` records the server instead (`servers()`), checking only port and
    encryption. M9's `mail_send` is JS and nodemailer, so it does not use this. But §5.1's rules
    are the same, and `connect.js` (M8.1) should match `relay.rs` case for case.
  - **M3.4: templates can follow joins, and now see secrets.** Template validation uses
    `sc_action::template_shape`, which is the action shape plus every Secret field.
    `render_event_template` opens a join path that ends on a secret
    (`secrets::reveal_join_value`). Other formulas (`only_if`, `where`, assignments, ownership,
    calculated fields) still cannot name a secret. Use case C is therefore a plain
    `send_email` trigger on the table that refers to `mail_accounts`, with no code-body step.
  - **Trigger settings marked `secret()` are now masked in the admin REST API too.** This
    covers list, create and update (`ActionRegistry::redact_trigger` / `merge_trigger_secrets`),
    not only the MCP tools. Backups still write the stored value.
  - **Follow-ups:** a `send_email` *workflow step*'s `smtp_password` is not masked in the
    workflow editor (steps are versioned JSON, with no per-action redaction yet). A literal
    password containing `{{` cannot be typed, because the setting is a template; put it in a
    Secret field instead.

## M4 — Modules declare tables (core)

- [x] **M4.1** Design: a `tables` facility in the plugin object.
  - **Shape:** `tables: [{ name, label, fields: [{ name, type, label, required, unique, references,
    secret, file_store, default }], unique: [[…]], ownership: "<formula>",
    min_role_read, min_role_write }]`.
  - Python can wait. Write the shape into TECHNICAL_DESIGN §11 before building it.
- [x] **M4.2** Install and upgrade: create the tables that are missing and add the fields that are
  missing, idempotently, through the catalog (not raw SQL), so the tables are ordinary tables:
  triggers, live streams, REST, ownership.
  - **Never drop** a table or field on upgrade or uninstall. Fields an admin added are left alone.
  - A declared field whose type no longer matches is reported on the module card, not changed.
- [x] **M4.3** The tables are marked as the module's: a badge in the table list, and the declared
  fields can't be deleted while the module is installed. The admin can still add fields, edit
  rows and set triggers.
- [x] **M4.4** Ownership formulas that join through other module tables. Check that
  `accountⱵowner` and `messageⱵaccountⱵowner` work as read rules and write rules.
  - Answer the question from §3: can some fields be written by server-side code only? If the
    ownership model can't do it cheaply, write down that the module re-checks them.
- [x] **M4.5** The module needs a file store for `mail_attachments.file`. Either it declares one
  in the facility, or it uses the default store under a `mail/` folder. Decide and document.
- [x] **M4.6** `bundled_catalog` test: a manifest's `supplies` lists the tables.
  `crates/sc-module/tests/` gets an install-twice-is-a-no-op test and an upgrade-adds-a-field
  test.

  **Done (2026-10-21).** Decisions worth knowing for M5–M9 (TECHNICAL_DESIGN §15.1b):
  - **M4.1: the shape** is as above, plus `description`, `file_folder`, and `tables` may be a
    function of the configuration. `type` is a feldspar type name (`text`, `int`, `bool`,
    `timestamp`, `json`, …), not v1's. `default` is a JSON value, or an expression as
    `"=<expr>"`, and needs `required`. Unknown keys are refused, so a typo is an issue on the card.
  - **M4.2: every module table gets an `id` int primary key** that numbers itself, and a
    declaration may not name one. `ModuleSet::load` syncs on **every** load (install, upgrade,
    configuration change, restart), in one `schema_edit` batch per module.
    - Nothing is dropped or altered. A changed type is an issue on the card.
    - A field that an upgrade adds is added **nullable**, with no default.
    - `label`, `ownership` and the roles are applied only when the table is created; after that
      they belong to the admin.
  - **M4.3: the claim is in memory, not stored.** `Catalog::set_module_tables` gets the loaded
    set's claims after each reload. `schema_edit::apply` refuses dropping a claimed table or a
    declared field, naming the module. Admin-added fields are not claimed. Uninstalling releases
    the claim and keeps the tables. `table_json` has `module`/`module_fields`; the list shows a
    badge, the field list disables Delete, and the module card lists `tables`. Changing a declared
    field's label is allowed; changing its type is not blocked (the edit form sends the whole
    field, so it can't be told apart cheaply). The upgrade then reports the mismatch.
  - **M4.4: joined rules work for reads and writes** (`sc-module/tests/module_tables.rs`), with
    `user.role` (the user object has no `role_id`). The accounts rule uses `_read` to be readable
    more widely than writable. **No field-level write rules:** M8/M9's actions re-check, and the
    service overwrites its own columns.
  - **M4.5: a file field names a store.** If no store of that name exists, a local one is created
    at `sc_files::suggest_local_dir(name)`. To use S3, the admin creates a store of that name
    first, or edits the created one. M6's `ctx.files` should accept only stores that the module's
    declared file fields name. The mail module should declare `file_store: "mail_files"` (or
    similar) with `file_folder: "attachments"`.
  - **M4.6: `feldspar-module.json` gets `tables: [names]`.** The card shows it ("Installing creates
    the tables …"). The reader refuses a listed table that no `supplies` line names as a whole
    word. M7.1's manifest must list all four mail tables.
  - **Python modules** can't declare tables yet. The declaration is plain data, so a
    `PyModuleSet` only needs to pass it to `tables::sync`.

## M5 — Module services (core)

- [ ] **M5.1** Design (TECHNICAL_DESIGN §11, next to "a module call is request/response").
  - **Shape:** `services: { name: { start: async (ctx) => stop } }`. Started after `onLoad` and
    after every settings change or worker restart. `stop` is awaited on uninstall or reload.
  - **`ctx.db`:** reads and writes **only the tables the module declared itself** (M4). Any other
    table is refused with a clear error. Writes go through the row layer, so triggers and live
    streams fire. The authority is "module", recorded as such in the event log.
  - **`ctx.secrets`:** decrypted Secret values of the module's own tables only.
  - `ctx.files`: M6's write API.
  - `ctx.runTrigger`, if cheap.
  - `ctx.log`.
  - `ctx.onTableChange(table, cb)`, if the live hub makes it cheap (§5.5).
  - The admin sees "runs in the background and writes to its own tables" next to the Install
    button. No new permission key is needed, because the authority is limited to the module's own
    tables.
- [ ] **M5.2** The host: a service's calls into the host come through the same ask channel as
  actions, with their own call context and no 120 s call bound. The JS slice bound (10 s) and the
  heap bound (256 MB) still apply.
  - Look at `module-host.mjs` `ask`/`running` and `deno/worker.rs` (around lines 1026–1052).
  - **A crash** (an exception escaping `start`, or the worker dying) is logged. The service is
    restarted with backoff, and its status is shown on the module card.
- [ ] **M5.3** Check that a module's sockets survive between calls and through service restarts,
  as the mqtt design claims (`deno/mod.rs` 17–24). Find out whether a module **has its own
  worker** or shares one: with 100 accounts' IDLE connections, other modules' calls must not wait
  behind mail parsing. If they would, run modules that have services on their own worker.
- [ ] **M5.4** Tests: a fixture module whose service inserts a row every 100 ms. Check that the
  rows arrive, that a live subscriber sees them, that writing to an undeclared table is refused,
  and that the service is restarted after a crash and stopped on uninstall.
- [ ] **M5.5** Docs in `docs/tutorial-modules.md`.

## M6 — File writes from JS modules (core)

- [ ] **M6.1** `ctx.files` (services) and the action context's equivalent:
  - `write(store, path, bytes | stream, { contentType }) → path`, streaming, so a 20 MB attachment
    is not one JS string;
  - `read(store, path)`;
  - `remove`.

  Limit it to the module's declared store or folder (M4.5). Python's `sc.fs` is the model
  (`docs/TODO-post-mvp-19.md` §180–198).
- [ ] **M6.2** Tests, including refusal outside the allowed store or folder, and a 20 MB write
  within the heap bound.

## M7 — The parser (module, no core dependencies)

- [ ] **M7.1** Scaffold `plugins/imap/`:
  - `package.json` with name `@feldspar/imap` and dependencies `imapflow`, `mailparser`,
    `nodemailer`, all pinned;
  - `feldspar-module.json`: language `javascript`, `installs` matching the dependencies,
    `permissions: { "net": ["*"] }`, and `supplies` listing the tables, the service and the
    actions;
  - `index.js`.

  `cargo test -p sc-module --test bundled_catalog` must pass. Add a row to `plugins/README.md`.
- [ ] **M7.2** `parse.js`: `parse(source: Buffer, { internalDate, flags, size }) → { message,
  attachments }`, implementing §5.4. A pure function, with no IMAP and no host.
- [ ] **M7.3** `parse-large.js`: the over-the-cap path, given `BODYSTRUCTURE` and a part fetcher
  (`(part) => Buffer`), so it can be tested with a fake fetcher.
- [ ] **M7.4** `tnef.js`: decode a TNEF stream into `{ body_text, body_rtf, attachments: [{name,
  type, data}] }`, with compressed RTF (MELA/LZFu) and a simple RTF-to-text conversion.
  - Evaluate npm `node-tnef` or a similar package against the GPL TNEF samples first. Port from
    `tnef.py` only if none works.
  - Accept it when every `.tnef` sample in the corpus yields the attachment names listed in its
    `.list`.
- [ ] **M7.5** The corpus test, `npm test` in `plugins/imap`, runs on every message in M1.1 and M1.2:
  - **(a)** It never throws.
  - **(b)** Every message ends up with a non-empty `body_text`, `body_html` or placeholder.
  - **(c)** No NUL byte in any value.
  - **(d)** For Stalwart's samples, subject, from, the text body (whitespace normalised) and the
    attachment count agree with the expected `.json`.
  - **(e)** Optional: a differential report against Python's `email` package, `python3 -I` with
    `policy.default`, comparing the same four fields.

  Every disagreement in (d) or (e) is either fixed or listed in `test/known-differences.md`
  with a reason. It then becomes an assertion.
- [ ] **M7.6** Performance: the whole corpus parses in under a minute. A 25 MB mail stays within
  the heap bound.

## M8 — Sync engine, ingest mode (use case A)

- [ ] **M8.1** `connect.js`: the connection rules (§5.1), as one function used by IMAP, SMTP and
  `mail_test_account`.
  - Unit tests: allowed and refused ports; a private, loopback or link-local address refused;
    a name that resolves to a public and a private address refused.
- [ ] **M8.2** `account.js`: one account's lifecycle:
  - connect, then discover folders (SPECIAL-USE flags first, then common names, §7);
  - upsert `mail_folders`;
  - IDLE on the first watched folder; other watched folders polled every 2 min;
  - reconnect with backoff; `auth_failed` handling; status fields.
- [ ] **M8.3** `sync.js`: one folder's new mail (§5.2), in batches of 50:
  - parse (M7); insert the messages and attachments, storing the files through M6;
  - move to `processed_folder` or `error_folder` after the batch;
  - advance `last_uid`;
  - `uidvalidity` change handling (ingest: match by `message_id`, never delete);
  - `start_from` handling (`UID SEARCH SINCE`).
- [ ] **M8.4** Flags and expunges (§5.3). In ingest mode, flags are updated and expunges recorded,
  never deleted.
- [ ] **M8.5** `service.js`: the module's service (M5).
  - Load the enabled accounts and run one `account.js` per account.
  - React to account changes (§5.5). Stop cleanly.
- [ ] **M8.6** Actions for ingest mode: `mail_test_account`, `mail_sync_now`, `mail_set_flags`,
  `mail_attachment` (a no-op when the file is already stored).
- [ ] **M8.7** Integration tests against Dovecot in Docker:
  - **ingest:** an existing mailbox of 1,000 mails; new mail during IDLE appears within 5 s;
  - **reconnects:** a server restart during a batch loses and duplicates nothing;
  - **uidvalidity:** a reset (recreate the mailbox);
  - **bad mail:** a broken mail gets `import_error` and doesn't block the batch;
  - **processed folder:** the mail is moved, and its row stays;
  - **ownership:**
    - the shared account's mail is visible to a staff user;
    - it is not visible to a user below `min_role`;
    - it is never visible through another user's personal account;
    - `imap_password` never appears in REST output.
- [ ] **M8.8** The order example as a test and a tutorial section: an Insert trigger on
  `mail_messages` filtered to one account creates an `orders` row.

## M9 — Mirror mode and the client actions (use cases B and D)

- [ ] **M9.1** Mirror sync:
  - all folders; INBOX by IDLE, the others polled every 5 min;
  - newest first, with full bodies for `body_days` and headers only for older mail;
  - slow backfill (a few hundred headers a minute per account);
  - expunges delete rows; `uidvalidity` resets rebuild the folder.
- [ ] **M9.2** Actions: `mail_fetch_body`, `mail_move`, `mail_delete`, `mail_create_folder`,
  `mail_save_draft`, `mail_attachment` (store when first opened).
- [ ] **M9.3** `mail_send` (§5.7), with nodemailer:
  - the Sent copy;
  - threading headers;
  - draft removal;
  - the `send_role` check for shared accounts.
- [ ] **M9.4** `thread_key`: the root of `References`, then `In-Reply-To`, then the message's own
  `message_id`. Index it, so the client can group conversations.
- [ ] **M9.5** Integration tests:
  - **isolation:** two users with two accounts each never see each other's mail or accounts, by
    REST, GraphQL or live subscription;
  - **flags:** a flag set in the app appears on the server, and one set on the server appears in
    the app;
  - **send:** a reply arrives at the SMTP sink with the right threading headers and lands in Sent;
  - **delete:** delete to Trash and permanent delete.
- [ ] **M9.6** Connection budget: the number of IMAP connections per account never goes above 2,
  even with the app calling actions in parallel. Queue on the short-lived connection.

## M10 — Docs, live servers, later

- [ ] **M10.1** `docs/tutorial-mail.md`:
  - shared mailbox to orders (A, C);
  - building a client on the tables (B, D): `useLiveRows` on `mail_messages`, rendering
    `body_html` in a sandboxed iframe with remote images blocked by default, rewriting `cid:`
    references, compose with `mail_send`.

  Add the module to the Modules catalog.
- [ ] **M10.2** A manual live-server test matrix, with results recorded in this file:
  - Gmail (app password), Microsoft 365, iCloud, Fastmail;
  - GMX, web.de, IONOS, Strato;
  - Dovecot, Exchange on-premises if available.

  Login, IDLE, new mail, flags, send, Sent copy.
- [ ] **M10.3** OAuth2 / XOAUTH2 per account (Gmail, Microsoft 365), built on the core OAuth work
  (`sc-server/src/oauth.rs`). The tokens are Secret fields. *(later)*
- [ ] **M10.4** Search: server-side `UID SEARCH` for an action, plus Postgres full-text search over
  `mail_messages`. *(later)*
- [ ] **M10.5** Risk scoring after `risk.py`'s ideas. CalDAV, and answering invitations. *(later)*
