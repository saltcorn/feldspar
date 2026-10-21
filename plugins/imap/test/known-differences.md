# Known differences (TODO-imap.md M7.5)

`parse.js` disagrees with the reference parsers on the mails below, on purpose or for a reason
written here. `corpus.test.mjs` asserts this file:

- **(d) Stalwart:** the set of disagreements is exactly the table below. A new one fails the test
  until it is fixed or added here with a reason; one that went away fails it too, until its row is
  removed.
- **(e) Python's `email` package:** too many to list one by one, because Python answers different
  questions (below). The counts are a ratchet. The test fails when a count goes up, and should be
  lowered here when one goes down.

Run `node test/corpus.mjs --verbose` (and `--python`) to see every disagreement with both values.

## Reasons

| reason | |
|---|---|
| inline message | An inline `message/rfc822` (no `attachment` disposition, including a `multipart/digest`'s parts and one wrongly base64-encoded) is opened and its text added to the body: §5.4, for Exchange journal reports. Stalwart, being JMAP, lists it as an attachment. An attached one (`attachment`) is an `.eml` attachment in both |
| inline media | An image, audio or video part in a `multipart/mixed` is part of the body to JMAP, which shows it in place. Here it is an attachment row, `inline` only when it has a Content-ID (§5.4), because a client needs a stored file to show it at all |
| 8-bit | Bytes that are not valid in the declared charset, or raw 8-bit in a header, are read as windows-1252 (latin-1's superset) or in the body's charset (§5.4). Stalwart writes U+FFFD. Ours reads `Frösche` where Stalwart has `Fr�sche` |
| bad bytes | Damaged text (an odd byte at the end of UTF-16, base64 junk after the padding) is decoded differently. Neither result is the sender's text |
| boundaries | Deliberately broken MIME boundaries (a boundary that is a prefix of another, a delimiter mid-line, an inner multipart closed by the outer delimiter, no boundary at all). `@zone-eu/mailsplit`, the splitter mailparser runs on, resolves them differently from Stalwart. No text is lost: what Stalwart reads as structure ends up in our body text |
| dropped alternative | A `text/enriched` alternative next to a `text/plain` one is dropped as a second rendering of the same text. Stalwart lists it as an attachment |
| no @ | `From: moderator` has no address. Ours has `from_addr` empty |
| garbage line | The header block starts with a line that has no colon and folds into `Subject:`. Ours reads them as one broken line |

## (d) Stalwart

| mail | field | reason |
|---|---|---|
| `stalwart/legacy/010.eml` | subject | 8-bit |
| `stalwart/legacy/020.eml` | subject | 8-bit |
| `stalwart/legacy/022.eml` | subject | 8-bit |
| `stalwart/legacy/027.eml` | subject | 8-bit |
| `stalwart/legacy/030.eml` | attachments | inline media |
| `stalwart/legacy/032.eml` | attachments | inline media |
| `stalwart/legacy/033.eml` | text | 8-bit |
| `stalwart/legacy/035.eml` | attachments | inline media |
| `stalwart/legacy/037.eml` | attachments | inline media |
| `stalwart/legacy/039.eml` | attachments | inline media |
| `stalwart/legacy/044.eml` | attachments | inline media |
| `stalwart/legacy/048.eml` | attachments | inline media |
| `stalwart/legacy/052.eml` | subject | 8-bit |
| `stalwart/legacy/052.eml` | text | 8-bit |
| `stalwart/malformed/001.eml` | attachments | boundaries |
| `stalwart/malformed/002.eml` | attachments | boundaries |
| `stalwart/malformed/004.eml` | text | boundaries |
| `stalwart/malformed/007.eml` | text | boundaries |
| `stalwart/malformed/009.eml` | text | boundaries |
| `stalwart/malformed/010.eml` | attachments | boundaries |
| `stalwart/malformed/011.eml` | text | boundaries |
| `stalwart/malformed/012.eml` | attachments | inline message |
| `stalwart/malformed/016.eml` | text | boundaries |
| `stalwart/malformed/020.eml` | attachments | boundaries |
| `stalwart/malformed/020.eml` | text | boundaries |
| `stalwart/malformed/021.eml` | attachments | boundaries |
| `stalwart/malformed/021.eml` | text | boundaries |
| `stalwart/malformed/023.eml` | attachments | boundaries |
| `stalwart/malformed/023.eml` | text | boundaries |
| `stalwart/regressions/decode-problems.eml` | text | 8-bit |
| `stalwart/regressions/doubly-encoded-message.eml` | attachments | inline message |
| `stalwart/regressions/empty-nested-message-base64.eml` | attachments | inline message |
| `stalwart/regressions/empty-nested-message.eml` | attachments | inline message |
| `stalwart/regressions/empty-nested-message-in-multipart.eml` | attachments | inline message |
| `stalwart/regressions/encoded-message-without-header.eml` | attachments | inline message |
| `stalwart/regressions/fallback-delimiter-mid-line.eml` | text | boundaries |
| `stalwart/regressions/garbage-line-before-fold.eml` | subject | garbage line |
| `stalwart/regressions/inner-multipart-closed-by-outer.eml` | text | boundaries |
| `stalwart/regressions/mixed-image.eml` | attachments | inline media |
| `stalwart/regressions/mixed-image-inline.eml` | attachments | inline media |
| `stalwart/regressions/mixed-message.eml` | attachments | inline message |
| `stalwart/regressions/mixed-message.eml` | text | inline message |
| `stalwart/regressions/mixed-message-inline.eml` | attachments | inline message |
| `stalwart/regressions/mixed-message-inline.eml` | text | inline message |
| `stalwart/regressions/mixed-text-image-html.eml` | attachments | inline media |
| `stalwart/regressions/nested-encoded-message.eml` | attachments | inline message |
| `stalwart/regressions/nested-encoded-message.eml` | text | inline message |
| `stalwart/regressions/nested-headers-cut-by-delimiter.eml` | attachments | inline message |
| `stalwart/regressions/nested-inline-message.eml` | attachments | inline message |
| `stalwart/regressions/nested-inline-message.eml` | text | inline message |
| `stalwart/regressions/unopened-multiparts.eml` | attachments | boundaries |
| `stalwart/regressions/utf16-dangling-byte.eml` | text | bad bytes |
| `stalwart/rfc/002.eml` | text | inline message |
| `stalwart/rfc/003.eml` | attachments | dropped alternative |
| `stalwart/rfc/004.eml` | attachments | inline message |
| `stalwart/rfc/004.eml` | text | inline message |
| `stalwart/rfc/007.eml` | attachments | inline message |
| `stalwart/rfc/009.eml` | attachments | inline message |
| `stalwart/rfc/009.eml` | text | inline message |
| `stalwart/thirdparty/002.eml` | text | bad bytes |
| `stalwart/thirdparty/005.eml` | text | boundaries |
| `stalwart/thirdparty/007.eml` | attachments | inline message |
| `stalwart/thirdparty/007.eml` | from | no @ |
| `stalwart/thirdparty/008.eml` | attachments | inline message |
| `stalwart/thirdparty/008.eml` | text | inline message |
| `stalwart/thirdparty/012.eml` | attachments | inline message |
| `stalwart/thirdparty/013.eml` | attachments | inline media |

## (e) Python's `email` package

`python3 -I` with `policy.default`, over the whole corpus (6,831 mails when it was last counted).
Python's answers are not the row's: `get_body(("plain",))` is the *first* plain part only, where
the row joins every body part (RFC 8621); and `iter_attachments()` counts the parts that are not
the chosen body, including body text parts and inline images, and never looks inside TNEF.

| field | at most | why, from sampling every category |
|---|---|---|
| (e) subject | 64 | Raw 8-bit subjects (mostly 2002 spam in Big5, GB2312, EUC-KR, TIS-620, ISO-8859-9): Python writes U+FFFD, ours decodes them, in the body's charset when it declares one. Also: malformed header lines Python takes as the start of the body (`Sub ject:`), a malformed ISO-2022-JP word Python leaves encoded, a NUL Python keeps |
| (e) from | 25 | `alice@example.com <bob@example.com>`: ours takes the address, Python the display name. Addresses without `@`, which ours drops. Quoted local parts (`"a@b"@c`), which ours unquotes. Raw 8-bit local parts |
| (e) text | 346 | Ours joins more parts (about 100: every part of the text view, inline messages opened); Python has U+FFFD where ours decoded (about 140); windows-1252 characters Python drops from a US-ASCII part (about 40); placeholders, header lines Python reads as body, the 400,000-character cut |
| (e) attachments | 200 | Python counts text parts after the first as attachments and inline `message/rfc822` as one; ours unpacks TNEF, lists `cid:` images and opens inline messages |
