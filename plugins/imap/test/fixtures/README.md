# Test mails

- `generated/` — awkward mails the public corpus lacks (TODO-imap.md M1.2), written by
  `generate.mjs`. **Do not edit them by hand**: change the generator, run
  `node test/fixtures/generate.mjs`, and commit both. `fixtures.test.mjs` fails when the two
  disagree.
- `manual/` — raw sources sent by hand from real clients (M1.3); the list is
  [`../MANUAL-SAMPLES.md`](../MANUAL-SAMPLES.md).

The public corpus (M1.1) is not here: `test/fetch-corpus.sh` fetches it into `test/corpus/`, which
is git-ignored.

## `generated/expected.json`

One entry per mail:

| key | |
|---|---|
| `file` | the `.eml` |
| `covers` | the lessons (`§6.n`) and parsing rules (`§5.4 …`) of TODO-imap.md it is for |
| `description` | what is awkward about it |
| `structure` | the content types of its MIME tree, in the order Python's `email` `walk()` yields them (a part with no Content-Type is `text/plain`). The test checks this with `python3 -I` |
| `expect` | what `parse.js` (M7.2) must make of it — see below |
| `tnef` | TNEF mails only: the RTF and the attachment names inside `winmail.dat` |

`expect` names fields of the `mail_messages` row (§3), and only the keys present are asserted:

| key | asserts |
|---|---|
| `subject`, `from_name`, `from_addr`, `message_id`, `in_reply_to`, `thread_key` | equal. `null` means empty. Message-IDs keep their angle brackets |
| `sent_at` | equal, as an ISO string; `null` means it fell back to INTERNALDATE |
| `to_addrs`, `cc_addrs`, `references` | equal lists |
| `subject_includes` | each string is in the subject |
| `body_text_includes`, `body_text_excludes` | each string is (or is not) in `body_text`, whitespace normalised |
| `body_html_includes`, `body_html_excludes` | the same for `body_html` |
| `body_html: null` | `body_html` is empty |
| `body_text_max_length` | `body_text` is at most this long |
| `placeholder` | `body_text` is the §5.4 placeholder for `encrypted`, `calendar` or `attachments_only`. The wording is `parse.js`'s |
| `attachments` | the attachment rows, in order: `filename`, `content_type`, `inline`, `cid` (no angle brackets), `size` and `sha256` of the decoded bytes. `[]` means none |
| `attachment_names` | only the filenames, in order |

Every mail, in addition, must not throw, must end with a non-empty body or placeholder, and must
have no NUL byte in any value (M7.5 a–c).
