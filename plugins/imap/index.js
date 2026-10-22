// **Mail: IMAP mailboxes synced into tables, and mail sent through an account's own SMTP
// server** — the bundled module `@feldspar/imap` (TODO-imap.md).
//
// The module owns its tables (§3): `mail_accounts`, `mail_folders`, `mail_messages` and
// `mail_attachments`. The server creates them when the module loads (tutorial-modules Step 7),
// and an application's own data refers to them: an `orders.message` key filled by an Insert
// trigger on `mail_messages`, say. Nobody maps mail fields onto columns of their own.
//
// The parts:
// - `parse.js`, `parse-large.js`, `tnef.js`: one mail's source into a row and its attachments (M7);
// - `connect.js`: where a connection may go (§5.1);
// - `sync.js`: one folder's new mail, flags and expunges into the tables (§5.2, §5.3);
// - `account.js`: one account's connection, IDLE, polling and reconnects;
// - `service.js`: the background service, one runner per enabled account (§5.5);
// - `actions.js`: the actions an application calls (§5.6).
//
// Both modes are here: ingest (use case A, a shared mailbox kept append-only) and mirror (B, a
// personal mail client's two-way copy), and sending from an account (C through `send_email`'s
// SMTP settings, D through `mail_send`).

"use strict";

// The service and the actions are required when first used, not here: `index.js` itself needs no
// installed package, so the server (and `bundled_catalog`'s test) can read its tables without
// `npm install` having run.

/** What the action types say, in a code query's `returns` type language (tutorial-modules): a
 * payload is the declared part of what an application sends, a `returns` what it gets back. They
 * type the application's generated `api.runMail…()` methods. */
const opt = (t) => ({ optional: t });
const IDS = { message_ids: opt({ array: "int" }), message_id: opt("int") };
const MOVED = { missing: { array: "int" }, not_on_server: { array: "int" } };
const ADDRESSES = opt({ union: ["string", { array: { union: ["string", { object: { name: opt("string"), address: "string" } }] } }] });
const ATTACHMENTS = opt({
  array: {
    union: [
      { object: { store: "string", path: "string", filename: opt("string"), content_type: opt("string"), cid: opt("string") } },
      { object: { attachment_id: "int", filename: opt("string"), cid: opt("string") } },
    ],
  },
});
const MESSAGE = {
  account_id: "int", to: ADDRESSES, cc: ADDRESSES, bcc: ADDRESSES, subject: opt("string"),
  text: opt("string"), html: opt("string"), attachments: ATTACHMENTS, in_reply_to: opt("int"),
};
const ACCOUNT = Object.fromEntries(
  ["email", "imap_host", "imap_security", "imap_user", "imap_password", "smtp_host", "smtp_security", "smtp_user", "smtp_password"]
    .map((k) => [k, opt("string")])
    .concat([["imap_port", opt("int")], ["smtp_port", opt("int")], ["account_id", opt("int")]]),
);

/** The actions (`actions.js`), by name, with what the trigger editor says of each and their types. */
const ACTIONS = {
  mail_test_account: {
    description: "Log in to an account's IMAP and SMTP servers with the settings given (saved or not). Answers { imap, smtp, error }.",
    payload: { object: ACCOUNT },
    returns: { object: { imap: "bool", smtp: opt("bool"), error: opt("string") } },
  },
  mail_sync_now: {
    description: "Sync an account now. Payload: { account_id, wait }.",
    payload: { object: { account_id: "int", wait: opt("bool") } },
    returns: { object: { queued: "bool", status: opt("string"), error: opt("string") } },
  },
  mail_set_flags: {
    description: "Mark messages read or unread, flagged or not, on the server and in the table. Payload: { message_ids, seen, flagged }.",
    payload: { object: { ...IDS, seen: opt("bool"), flagged: opt("bool") } },
    returns: { object: { updated: { array: "int" }, ...MOVED } },
  },
  mail_attachment: {
    description: "Store an attachment's file if it is not stored yet, and answer its File path. Payload: { attachment_id }.",
    payload: { object: { attachment_id: "int" } },
    returns: { object: { file: "string", stored: "bool" } },
  },
  mail_fetch_body: {
    description: "Fetch the body of a message stored with its headers only (mirror mode's older mail). Payload: { message_id }.",
    payload: { object: { message_id: "int" } },
    returns: { object: { id: "int", body_state: "string", fetched: "bool" } },
  },
  mail_move: {
    description: "Move messages to another folder of their account, on the server and in the table. Payload: { message_ids, target } (a path, or role:trash, role:archive, …).",
    payload: { object: { ...IDS, target: "string" } },
    returns: { object: { moved: { array: "int" }, ...MOVED } },
  },
  mail_delete: {
    description: "Delete messages: to Trash, or for good when permanent (or already in Trash). Mirror accounts. Payload: { message_ids, permanent }.",
    payload: { object: { ...IDS, permanent: opt("bool") } },
    returns: { object: { trashed: { array: "int" }, deleted: { array: "int" }, ...MOVED } },
  },
  mail_create_folder: {
    description: "Create a folder on a mirror account's server. Payload: { account_id, name, parent }.",
    payload: { object: { account_id: "int", name: "string", parent: opt("string") } },
    returns: { object: { id: "int", path: "string" } },
  },
  mail_save_draft: {
    description: "Save a draft in the account's Drafts folder, replacing an earlier one. Payload: { account_id, to, cc, bcc, subject, text, html, attachments, in_reply_to, replaces }.",
    payload: { object: { ...MESSAGE, replaces: opt("int") } },
    returns: { object: { id: opt("int"), uid: opt("int"), message_id: "string", replaced: opt("int") } },
  },
  mail_send: {
    description: "Send a message through the account's SMTP server and keep a copy in Sent. Payload: { account_id, to, cc, bcc, subject, text, html, attachments, in_reply_to, draft_id }.",
    payload: { object: { ...MESSAGE, draft_id: opt("int") } },
    returns: {
      object: {
        message_id: "string", id: opt("int"), saved: "bool", save_error: opt("string"),
        accepted: { array: "string" }, rejected: { array: "string" },
        draft_deleted: opt("bool"), warnings: opt({ array: "string" }),
      },
    },
  },
};

/** v1's `actions(configuration)`: the module's settings reach an action this way, because the
 * `configuration` in an action's argument is its trigger's, not the module's. */
const actions = (configuration) =>
  Object.fromEntries(
    Object.entries(ACTIONS).map(([name, { description, payload, returns }]) => [
      name,
      { description, payload, returns, run: (args) => require("./actions.js").actions[name]({ ...args, settings: configuration || {} }) },
    ]),
  );

/** Who may see a row of a table that belongs to an account: the account's owner, or, for a
 * shared account (no owner), every user at or above its `min_role` (§3). */
const viaAccount = (path) => `${path}Ⱶowner === user.id || (${path}Ⱶowner === null && user.role <= ${path}Ⱶmin_role)`;

const tables = [
  {
    name: "mail_accounts",
    label: "Mail accounts",
    description: "IMAP and SMTP accounts. An account with no owner is shared, and readable by every user at or above its minimum role.",
    fields: [
      { name: "owner", references: "users", label: "Owner", description: "Empty for a shared account" },
      { name: "min_role", type: "int", label: "Minimum role to read", description: "Shared accounts: the least role that can read its mail" },
      { name: "send_role", type: "int", label: "Minimum role to send", description: "Shared accounts: the least role that can send from it" },
      { name: "label", type: "text", label: "Label" },
      { name: "email", type: "text", label: "Email address" },
      { name: "display_name", type: "text", label: "Display name" },
      { name: "mode", type: "text", required: true, default: "ingest", label: "Mode", description: "ingest: keep every mail; mirror: a two-way copy of the mailbox" },
      { name: "imap_host", type: "text", label: "IMAP host" },
      { name: "imap_port", type: "int", label: "IMAP port" },
      { name: "imap_security", type: "text", label: "IMAP security", description: "tls (993) or starttls (143)" },
      { name: "imap_user", type: "text", label: "IMAP user" },
      { name: "imap_password", secret: true, label: "IMAP password" },
      { name: "smtp_host", type: "text", label: "SMTP host" },
      { name: "smtp_port", type: "int", label: "SMTP port" },
      { name: "smtp_security", type: "text", label: "SMTP security", description: "tls (465) or starttls (587)" },
      { name: "smtp_user", type: "text", label: "SMTP user" },
      { name: "smtp_password", secret: true, label: "SMTP password" },
      { name: "auth", type: "text", required: true, default: "password", label: "Authentication" },
      { name: "folders", type: "json", label: "Folders", description: 'Ingest: the folders to watch (default ["INBOX"])' },
      { name: "processed_folder", type: "text", label: "Move imported mail to" },
      { name: "error_folder", type: "text", label: "Move failed mail to" },
      { name: "start_from", type: "timestamp", label: "Import mail from", description: "Ingest: only mail on or after this date. Empty means all of it" },
      { name: "body_days", type: "int", required: true, default: 90, label: "Days of full bodies", description: "Mirror: older mail keeps its headers only until it is opened" },
      {
        name: "save_sent",
        type: "bool",
        label: "Copy sent mail to Sent",
        description: "Empty: yes, except for Gmail and Microsoft 365, whose servers file sent mail themselves",
      },
      { name: "enabled", type: "bool", required: true, default: true, label: "Enabled" },
      { name: "status", type: "text", label: "Status" },
      { name: "last_error", type: "text", label: "Last error" },
      { name: "last_sync_at", type: "timestamp", label: "Last synced" },
    ],
    // Readable like the other tables; writable only as one's own account (§3, M4.4).
    ownership: "owner === user.id || (_read && owner === null && user.role <= min_role)",
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "mail_folders",
    label: "Mail folders",
    fields: [
      { name: "account", references: "mail_accounts", required: true, label: "Account" },
      { name: "path", type: "text", required: true, label: "Path" },
      { name: "name", type: "text", label: "Name" },
      { name: "role", type: "text", label: "Role", description: "inbox, sent, drafts, trash, spam, archive, or empty" },
      { name: "delimiter", type: "text", label: "Delimiter" },
      { name: "uidvalidity", type: "int", label: "UIDVALIDITY" },
      { name: "last_uid", type: "int", label: "Last UID" },
      // A 63-bit number, more than a JSON number holds exactly.
      { name: "highestmodseq", type: "text", label: "HIGHESTMODSEQ" },
      { name: "unread", type: "int", label: "Unread" },
      { name: "total", type: "int", label: "Total" },
      { name: "sync", type: "bool", label: "Synced" },
      { name: "backfill_uid", type: "int", label: "Backfill below UID", description: "Mirror: older mail below this UID is still to be stored; 0 when done" },
    ],
    unique: [["account", "path"]],
    ownership: viaAccount("account"),
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "mail_messages",
    label: "Mail messages",
    fields: [
      { name: "account", references: "mail_accounts", required: true, label: "Account" },
      { name: "folder", references: "mail_folders", label: "Folder" },
      { name: "uid", type: "int", label: "UID" },
      { name: "message_id", type: "text", label: "Message-ID" },
      { name: "in_reply_to", type: "text", label: "In-Reply-To" },
      { name: "references", type: "json", label: "References" },
      { name: "thread_key", type: "text", label: "Thread" },
      { name: "subject", type: "text", label: "Subject" },
      { name: "from_name", type: "text", label: "From (name)" },
      { name: "from_addr", type: "text", label: "From" },
      { name: "to_addrs", type: "json", label: "To" },
      { name: "cc_addrs", type: "json", label: "Cc" },
      { name: "reply_to", type: "json", label: "Reply-To" },
      { name: "sent_at", type: "timestamp", label: "Sent" },
      { name: "internal_date", type: "timestamp", label: "Received" },
      { name: "snippet", type: "text", label: "Snippet" },
      { name: "body_text", type: "text", label: "Text" },
      { name: "body_html", type: "text", label: "HTML" },
      { name: "size", type: "int", label: "Size" },
      { name: "has_attachments", type: "bool", label: "Has attachments" },
      { name: "seen", type: "bool", label: "Seen" },
      { name: "flagged", type: "bool", label: "Flagged" },
      { name: "answered", type: "bool", label: "Answered" },
      { name: "draft", type: "bool", label: "Draft" },
      { name: "body_state", type: "text", label: "Body", description: "full, headers (mirror mode's older mail, until opened) or too_large" },
      { name: "import_error", type: "text", label: "Import error" },
    ],
    unique: [["folder", "uid"]],
    // A client groups conversations by `thread_key` (M9.4).
    indexes: [["thread_key"]],
    ownership: viaAccount("account"),
    min_role_read: 1,
    min_role_write: 1,
  },
  {
    name: "mail_attachments",
    label: "Mail attachments",
    fields: [
      { name: "message", references: "mail_messages", required: true, label: "Message" },
      { name: "part", type: "text", label: "Part", description: "The IMAP part number, e.g. 2.1" },
      { name: "filename", type: "text", label: "File name" },
      { name: "content_type", type: "text", label: "Content type" },
      { name: "size", type: "int", label: "Size" },
      { name: "cid", type: "text", label: "Content-ID" },
      { name: "inline", type: "bool", label: "Inline image" },
      { name: "file", file_store: "mail_files", file_folder: "attachments", label: "File" },
    ],
    ownership: viaAccount("messageⱵaccount"),
    min_role_read: 1,
    min_role_write: 1,
  },
];

/** The module's settings: what only an admin decides, for every account (`connect.js`). */
const configuration_workflow = () => ({
  steps: [
    {
      name: "Servers",
      form: {
        fields: [
          {
            name: "trusted_servers",
            label: "Trusted servers",
            type: "String",
            fieldview: "textarea",
            sublabel:
              "Mail servers inside your own network, one per line, as host or host:port. Only these may " +
              "resolve to a private address, and a port given here is allowed besides 993/143 and 465/587.",
          },
          {
            name: "ca_certificates",
            label: "Extra CA certificates",
            type: "String",
            fieldview: "textarea",
            sublabel: "PEM certificates of your internal CA, used to check the trusted servers above instead of the public ones. Certificates are always checked.",
          },
        ],
      },
    },
  ],
});

module.exports = {
  sc_plugin_api_version: 1,
  tables,
  configuration_workflow,
  services: {
    sync: {
      description: "Keeps each enabled account connected, and writes new mail and flag changes into the tables",
      start: (ctx) => require("./service.js").start(ctx),
    },
  },
  actions,
};
