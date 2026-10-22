# Mails to send by hand (TODO-imap.md M1.3)

The public corpus (M1.1) is old and mostly from Unix mailers, and the generated mails (M1.2) are
what we *think* clients write. This is the list of what real clients write, which only someone with
the client can produce.

## How to take a sample

1. Send the mail **to an account whose raw source you can download** (Dovecot, Fastmail, Gmail's
   "Show original" → "Download original"). For Exchange-internal TNEF, the recipient must be in the
   same Exchange organisation as the sender, or the server converts it before it leaves.
2. Save the raw source, unchanged — not re-saved by another client — as
   `test/fixtures/manual/<client>-<case>.eml`, with the names below.
3. Replace any personal addresses, names and signatures in the headers and body by hand, with a
   text editor that keeps the bytes (no re-encoding). Do not touch base64 parts. If an
   attachment is private, send a different one instead.
4. Tick the box here, and note the client's version.

`fixtures.test.mjs` checks that every file in `manual/` is a `.eml` named in this list.

## The cases

For each client:

| case | what to send |
|---|---|
| `plain` | plain text only (switch the composer to plain text), with an umlaut and a € in subject and body |
| `html` | formatted text: bold, a link, a bulleted list, a coloured word |
| `inline-image` | HTML with a picture pasted into the body (not attached) |
| `two-attachments` | a PDF and a picture, attached, with a name containing a space and an umlaut |
| `reply` | a reply to one of the above, quoting it |
| `forward-attachment` | a forward **as attachment** (`.eml` / "Forward as attachment") |
| `calendar-invite` | a meeting invitation to one other attendee |

## The clients

| client | prefix | plain | html | inline-image | two-attachments | reply | forward-attachment | calendar-invite | version |
|---|---|---|---|---|---|---|---|---|---|
| Outlook desktop (Windows), to Exchange-internal (TNEF) | `outlook-desktop` | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | |
| Microsoft 365, Exchange-internal (TNEF) | `m365` | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | |
| Outlook on the web | `outlook-web` | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | |
| Gmail web | `gmail-web` | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | |
| Apple Mail (macOS) | `apple-mail` | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | |
| iOS Mail | `ios-mail` | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | |
| Thunderbird | `thunderbird` | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | [ ] | |

The file names, in full (so the test can find each one in this list):

- `outlook-desktop-plain.eml`, `outlook-desktop-html.eml`, `outlook-desktop-inline-image.eml`,
  `outlook-desktop-two-attachments.eml`, `outlook-desktop-reply.eml`,
  `outlook-desktop-forward-attachment.eml`, `outlook-desktop-calendar-invite.eml`
- `m365-plain.eml`, `m365-html.eml`, `m365-inline-image.eml`, `m365-two-attachments.eml`,
  `m365-reply.eml`, `m365-forward-attachment.eml`, `m365-calendar-invite.eml`
- `outlook-web-plain.eml`, `outlook-web-html.eml`, `outlook-web-inline-image.eml`,
  `outlook-web-two-attachments.eml`, `outlook-web-reply.eml`,
  `outlook-web-forward-attachment.eml`, `outlook-web-calendar-invite.eml`
- `gmail-web-plain.eml`, `gmail-web-html.eml`, `gmail-web-inline-image.eml`,
  `gmail-web-two-attachments.eml`, `gmail-web-reply.eml`, `gmail-web-forward-attachment.eml`,
  `gmail-web-calendar-invite.eml`
- `apple-mail-plain.eml`, `apple-mail-html.eml`, `apple-mail-inline-image.eml`,
  `apple-mail-two-attachments.eml`, `apple-mail-reply.eml`,
  `apple-mail-forward-attachment.eml`, `apple-mail-calendar-invite.eml`
- `ios-mail-plain.eml`, `ios-mail-html.eml`, `ios-mail-inline-image.eml`,
  `ios-mail-two-attachments.eml`, `ios-mail-reply.eml`, `ios-mail-forward-attachment.eml`,
  `ios-mail-calendar-invite.eml`
- `thunderbird-plain.eml`, `thunderbird-html.eml`, `thunderbird-inline-image.eml`,
  `thunderbird-two-attachments.eml`, `thunderbird-reply.eml`,
  `thunderbird-forward-attachment.eml`, `thunderbird-calendar-invite.eml`

## What each is for

- **TNEF** (Outlook desktop, Microsoft 365): `winmail.dat` with an RTF or HTML-in-RTF body, and
  attachments inside it (§5.4, M7.4). The generated TNEF mails are written from the spec; these are
  the real thing.
- **inline-image**: whether the client writes `multipart/related` with a Content-ID, and how it
  quotes the `cid:` reference (§6.10).
- **two-attachments**: filename encodings — RFC 2231 or encoded words — per client.
- **reply**: `In-Reply-To` and `References` for `thread_key` (M9.4), and the quoting styles.
- **forward-attachment**: attached `message/rfc822` (§6.12).
- **calendar-invite**: `text/calendar` alone or with a text part, and `method=REQUEST` (§5.4).
