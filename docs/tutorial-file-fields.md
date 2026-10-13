# Tutorial: File fields — attachments on your tasks

Give a table a `File` field, open the table to a non-admin role, and upload and serve real
files from an application — with the folder and file-type rules enforced by the server, not by
the politeness of the client. **Everything below happens in a browser.**

This continues from [tutorial-react-todo.md](tutorial-react-todo.md): you have a server started
with `--base-domain localhost`, a `tasks` table, and a React `todo` app on
`http://todo.localhost:3032`. Nothing here depends on that app's specifics — any table and any
application will do — but the names below assume it.

## What a `File` field is

A `File` field is a **reference, not a blob**: the column stores a path (like
`tasks/report.pdf`), relative to a named **file store**, and the bytes live in the store. The
field can restrict *where* in the store its files live (a folder) and *what* they may be (a MIME
allow-list). Because the bytes are ordinary files on disk, they survive anything you do to the
metadata: deleting the field, or even dropping the table, never deletes a byte.

## Step 1 — A store for the uploads

Your app's *source* already lives in the `apps` store. Uploads should not live next to source
code, so create a second store under **File stores → New file store**:

| Field | Value |
|---|---|
| Name | `uploads` |
| Description | `User uploads` |
| Backend | `local` |
| **Directory** | `/srv/uploads` |
| **Create the directory if it does not exist** | ✔ |

Save. It is connected immediately, no restart.

## Step 2 — A role for the app's users

A freshly created table is **admin-only**, on both reads and writes; an application can only
serve it to its admin until you say otherwise. Saying otherwise takes a role to say it *to*.

Go to **Roles** and add one:

| Field | Value |
|---|---|
| Number | `40` |
| Name | `Member` |

Lower numbers are more privileged: admin is 1, public is 100, and your Member sits in between.
Then, under **Users → Add user**, create someone who holds it — say `member@example.com`, any
password, role **Member (40)**.

## Step 3 — Open the table to Members

Go to **Tables → tasks**. Above the fields there is a **Settings** card:

| Setting | Value |
|---|---|
| Who can read rows | `Member (40)` |
| Who can create, update and delete rows | `Member (40)` |

Press **Save settings**. The confirmation means what it says — *the new rules apply immediately,
no restart* — because a mounted application re-projects its API the moment a table's rules
change. Reads and writes are separate settings, so you could just as well have opened reads to
Members while keeping writes admin-only.

(**Forget settings** undoes this: the table returns to the closed admin-only default. It never
touches the table or its rows.)

## Step 4 — Add the `File` field

Still on **Tables → tasks**, in the **Fields** card, add a field:

| Field | Value |
|---|---|
| Name | `attachment` |
| Type | **file** (under *Field kinds*) |
| File store | `uploads` |
| Folder (optional) | `tasks` |
| Allowed MIME types (optional) | `["application/pdf", "image/png", "image/jpeg"]` |

The store is a pick-list of connected stores; the MIME list is entered as a JSON array. Press
**Add field**.

What you configured is a contract the server now enforces on every write, whoever makes it:

- the column may only hold a path **inside the `uploads` store**, under `tasks/` — a path that
  climbs out (`..`, an absolute path, another folder) is a 400 naming the field;
- the path's **extension** must map to an allowed MIME type — `notes.txt` is refused whatever
  `Content-Type` the request claims;
- a caller who is not an admin may only write the path of a file that **exists and is theirs**:
  one they uploaded, or one nobody owns that their role can reach. Somebody else's upload, or a
  path with no file at it, is a 400 naming the field — otherwise a user could put the path of
  another user's file in their own row and download it through that row;
- the `uploads` store can no longer be deleted while this field points at it.

## Step 5 — Rebuild the app

On the app's row under **Applications**, press **Build**.

The running app's API grew two endpoints the moment the field was created — files are addressed
**by table, row id and field name**, never by raw store path:

```
GET  /api/tasks/{id}/attachment             the bytes behind this row's attachment
POST /api/tasks/{id}/attachment/{filename}  upload; body = the file's bytes
```

The rebuild is for the *typed client*: `src/feldspar/client.ts` is regenerated from the app's
endpoints, and now carries

```ts
downloadTasksAttachment(id: number): Promise<Blob>;
uploadTasksAttachment(id: number, filename: string, body: BodyInit): Promise<...>;
```

and `TasksRow` gained `attachment?: string | null` — the stored path, like any other column.

## Step 6 — Upload from the app

Edit `src/pages/Tasks.tsx` (through the file manager or your own editor). Add a file input to
each task row:

```tsx
import { api, invalidate } from "../feldspar/hooks";

function AttachButton({ id }: { id: number }) {
  return (
    <input
      type="file"
      accept="application/pdf,image/png,image/jpeg"
      onChange={async (e) => {
        const file = e.target.files?.[0];
        if (!file) return;
        await api.uploadTasksAttachment(id, file.name, file);
        invalidate("tasks"); // the row now references the file; re-fetch the list
      }}
    />
  );
}
```

A `File` from an `<input type="file">` is a valid `BodyInit`, so it is handed to the client
as-is — no base64, no form encoding. The caller names only a **filename**; the folder comes from
the field, so the bytes land at `tasks/<filename>` in the `uploads` store and the row's
`attachment` column is set to that path. The `accept` attribute is a convenience for the file
picker; the allow-list you configured in step 4 is checked again on the server, from the
filename's extension.

Showing an attachment is a link — the browser sends the session cookie, so a plain URL works:

```tsx
{task.attachment && (
  <a href={`/api/tasks/${task.id}/attachment`} target="_blank" rel="noreferrer">
    {task.attachment}
  </a>
)}
```

(For programmatic use, `api.downloadTasksAttachment(id)` resolves to a `Blob`.)

Press **Build**, reload `todo.localhost:3032`, sign in as `member@example.com`, and attach a PDF
to a task. Then try a `.txt` file: the picker's filter will resist, and if you override it the
server answers 400 — *`attachment`: file `tasks/notes.txt` has MIME type `text/plain`, which is
not allowed* — and writes nothing.

## Step 7 — Who may fetch the bytes

Two rules govern a download, and **the stricter one always decides**:

1. **The table's read role.** The download endpoint requires the table's *read* role, the upload
   its *write* role — exactly like the row endpoints. Your Member clears both.
2. **The path's own rule.** Every file and folder in a store can carry a minimum role of its own
   (browse the store in the file manager and open a file's details — the **Minimum role** box).
   The rule is *path-cumulative*: to reach `tasks/report.pdf` a caller must clear the rule on
   the store, on `tasks`, and on `report.pdf` itself, so a folder's restriction covers
   everything beneath it and nothing inside can opt back out.

To see the second rule bite: in the file manager, browse `uploads`, select the `tasks` folder,
and set its minimum role to `1`. Your Member can still list tasks — the *table* still admits
them — but fetching any attachment is now refused, until you clear the folder's rule again. The
same file, admitted by one rule and refused by the other: whichever is stricter wins.

## Things that trip people up

- **The MIME check reads the filename, not the request.** `Content-Type` headers are what the
  sender *claims*; the extension is what the allow-list means. A file with no extension against
  a non-empty allow-list is refused.
- **An app never chooses the path.** Uploads name a single filename — the URL cannot even
  express a nested or traversing destination — and the folder comes from the field. If you need
  files sorted into sub-folders, that is the field's folder setting, not the caller's choice.
- **Uploading over the same filename replaces the bytes.** The path is the identity. Two rows
  that reference `tasks/report.pdf` reference the same file.
- **A path sent in a row's JSON has to be the caller's own file.** Setting `attachment` by
  `PUT`/`POST` works for a file the caller uploaded (through this endpoint or an upload slot), and
  for an admin with any file. Sending back the path a row already holds is always fine, so a
  whole-row save by somebody other than the uploader still works.
- **Metadata operations never delete bytes.** Forgetting a field, deleting a row, dropping the
  table: the files stay in the store. Delete them in the file manager when you mean it.
- **A store with a `File` field pointing at it refuses to be deleted**, naming the field. Forget
  the field first.
- **Admin row edits get a picker.** In the admin UI's row editor, a `File` field is a path box
  with a **Choose…** button browsing the field's store — the same reference, chosen instead of
  uploaded.

## What next

This tutorial opened the whole `tasks` table to the Member role. The next one closes it again
and hands each user back exactly the rows they *own*, through a one-line JavaScript **ownership
formula** — then pushes that same formula into Postgres as row-level-security policies you can
read in `psql`: [tutorial-ownership.md](tutorial-ownership.md).
