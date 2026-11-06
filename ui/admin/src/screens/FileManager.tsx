// The file manager: browse one store's tree, and act on what is in it.
//
// The MVP built `browseFiles`/`readFile`/`writeFile` — `readFile`'s UTF-8 `text`
// shortcut is commented in the server as being "for the text editor" — and then
// shipped no screen that called any of them. This is that screen, over the fuller
// operation set §1.4b added: create a folder, upload, download, rename, delete,
// edit a text file in place, and set the per-file access rule.
//
// It is a *file browser*, and the shape of that is not a matter of taste — it is
// what everyone already knows how to use:
//
// - **A selection, not a row of buttons.** Click selects, shift-click takes the
//   run between, ctrl-click adds one, Ctrl-A takes the lot, Escape drops it.
//   Anything that can be done to one thing can be done to four, from the badge
//   that appears above the listing. The arithmetic is in `fileSelection.ts` so it
//   can be asserted without a browser.
// - **One menu per row, not five buttons.** Five buttons per row is a wall of
//   noise that grows every time an operation is added, and the useful ones
//   (open, download) are the ones that were hardest to find in it.
// - **A search box that searches the tree.** `findFiles` walks the store
//   server-side, so finding a file two directories down is typing its name rather
//   than remembering where it was put. The results are listing entries, so they
//   are shown in the same table with the same columns.
// - **Columns worth having.** Size, modified, owner and the access rule that
//   actually reaches the entry — the last of which used to be reachable only by
//   opening a dialog per file, which is no way to notice that a folder is locked.
//
// One idea worth keeping in mind while reading: a file's *effective* minimum role
// is not the same as the rule set on it. A parent directory can be stricter, and
// the rule is cumulative down the path. The access column shows the effective
// one, muted where it is inherited, and the permissions dialog shows both — so an
// admin cannot conclude a file is reachable from a permissive rule on the file
// while its folder is locked.

import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Breadcrumb from "react-bootstrap/Breadcrumb";
import Button from "react-bootstrap/Button";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";
import Table from "react-bootstrap/Table";

import { api, downloadUrl, errorMessage, uploadFile } from "../api";
import type { GetFileMetaResponse } from "../client";
import { ideUrl, navigate } from "../App";
import {
  NOTHING_SELECTED,
  clickSelection,
  copyablePaths,
  formatModified,
  formatSize,
  keepPresent,
  parentDir,
  selectAll,
  selectionSummary,
  type FileEntry,
  type Selection,
} from "../fileSelection";
import { IconDots, IconFile, IconFolder, IconSearch } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import { OptionalRoleSelect } from "../roleSelect";
import { roleLabel, useRoles, type Roles } from "../roles";
import { T, useT } from "../i18n";

type Entry = FileEntry;

/** How long the search box waits after a keystroke before asking the server. A
 * name search walks the tree, so it is cheap but not free, and nobody wants a
 * request per character. */
const SEARCH_DEBOUNCE_MS = 250;

/** The parent of a store-relative directory path (`""` is the root). */
function parentOf(dir: string): string {
  const trimmed = dir.replace(/\/+$/, "");
  const slash = trimmed.lastIndexOf("/");
  return slash === -1 ? "" : trimmed.slice(0, slash);
}

/** Join a directory and a name into a store-relative path. */
function joinPath(dir: string, name: string): string {
  return dir ? `${dir}/${name}` : name;
}

export function FileManager({
  store,
  initialDir = "",
}: {
  store: string;
  /** The directory to open in; `""` is the store root. */
  initialDir?: string;
}) {
  const { t } = useT();
  const [dir, setDir] = useState(initialDir);
  const [entries, setEntries] = useState<Entry[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [editing, setEditing] = useState<{ path: string; text: string } | null>(null);
  const [permissions, setPermissions] = useState<GetFileMetaResponse | null>(null);
  const [selection, setSelection] = useState<Selection>(NOTHING_SELECTED);
  const [search, setSearch] = useState("");
  // What the *listing* is: a directory, or the results of a search across the
  // whole store. They are the same rows in the same table, so everything below
  // works on `entries` and only the header and the name cell differ.
  const [searched, setSearched] = useState<{ query: string; truncated: boolean } | null>(null);
  const roles = useRoles();
  const uploadInput = useRef<HTMLInputElement>(null);

  /** Replace the listing, dropping anything selected that is no longer in it. */
  const show = useCallback((next: Entry[]) => {
    setEntries(next);
    setSelection((current) =>
      keepPresent(
        current,
        next.map((e) => e.path),
      ),
    );
  }, []);

  const load = useCallback(
    async (target: string) => {
      setError(null);
      setSearched(null);
      try {
        show(await api.browseFiles(store, { dir: target }));
        setDir(target);
      } catch (err) {
        show([]);
        setError(errorMessage(err, `Could not list "${target || "/"}".`));
      }
    },
    [store, show],
  );

  useEffect(() => {
    void load(initialDir);
  }, [load, initialDir]);

  // The search box, debounced. An empty box is not a search for nothing — it is
  // the directory listing back, which is what pressing Escape or clearing the
  // box has to restore.
  useEffect(() => {
    const query = search.trim();
    if (!query) {
      if (searched) void load(dir);
      return;
    }
    const timer = setTimeout(() => {
      void (async () => {
        setError(null);
        try {
          // Rooted at the directory in view, so searching inside a folder
          // searches that folder — the breadcrumb says what is being searched.
          const found = await api.findFiles(store, { query, dir, max_results: null });
          show(found.entries);
          setSearched({ query, truncated: found.truncated });
        } catch (err) {
          show([]);
          setError(errorMessage(err, "Could not search the store."));
        }
      })();
    }, SEARCH_DEBOUNCE_MS);
    return () => clearTimeout(timer);
    // `searched` is deliberately not a dependency: it is what this effect sets,
    // and reacting to it would re-run the search on its own result.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [search, dir, store, show, load]);

  /** Re-run whatever is on screen — a directory listing, or the search. */
  const refresh = useCallback(async () => {
    const query = search.trim();
    if (!query) {
      await load(dir);
      return;
    }
    const found = await api.findFiles(store, { query, dir, max_results: null });
    show(found.entries);
    setSearched({ query, truncated: found.truncated });
  }, [dir, load, search, show, store]);

  /** Run a mutating action, then refresh — the shape every action here shares. */
  const act = async (what: string, run: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await run();
      await refresh();
    } catch (err) {
      setError(errorMessage(err, what));
    } finally {
      setBusy(false);
    }
  };

  const order = useMemo(() => (entries ?? []).map((e) => e.path), [entries]);
  const selected = useMemo(
    () => (entries ?? []).filter((e) => selection.selected.includes(e.path)),
    [entries, selection],
  );

  // Ctrl-A selects everything on screen and Escape clears — on the window,
  // because the listing is a table and a table is not a focusable thing to hang
  // a key handler on. Typing in the search box or a dialog is exempt: Ctrl-A
  // there means "select this text", which is what the browser would have done.
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      const typing =
        target instanceof HTMLInputElement ||
        target instanceof HTMLTextAreaElement ||
        target?.isContentEditable === true;
      if (typing) return;
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "a") {
        event.preventDefault();
        setSelection(selectAll(order));
      } else if (event.key === "Escape") {
        setSelection(NOTHING_SELECTED);
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [order]);

  const clickRow = (entry: Entry, event: React.MouseEvent) => {
    setSelection((current) =>
      clickSelection(order, current, entry.path, {
        shiftKey: event.shiftKey,
        ctrlKey: event.ctrlKey,
        metaKey: event.metaKey,
      }),
    );
  };

  /** Double-click: a folder opens, a text file opens in the editor. The same as
   * pressing the first item of the row's menu, which is where a person who did
   * not think to double-click will look. */
  const openEntry = (entry: Entry) => {
    if (entry.is_dir) {
      setSearch("");
      void load(entry.path);
    } else {
      void openEditor(entry);
    }
  };

  const makeFolder = () => {
    const name = window.prompt("New folder name");
    if (!name?.trim()) return;
    void act("Could not create the folder.", () =>
      api.makeDirectory(store, { path: joinPath(dir, name.trim()) }),
    );
  };

  const upload = (files: FileList | null) => {
    if (!files?.length) return;
    void act("Could not upload the file.", async () => {
      for (const file of Array.from(files)) {
        await uploadFile(store, joinPath(dir, file.name), file);
      }
    });
  };

  const rename = (entry: Entry) => {
    const next = window.prompt(`Rename "${entry.name}" to`, entry.name);
    if (!next?.trim() || next.trim() === entry.name) return;
    // Renamed where it is, which for a search hit is not the directory in view.
    void act("Could not rename it.", () =>
      api.renameFile(store, {
        from: entry.path,
        to: joinPath(parentDir(entry.path), next.trim()),
      }),
    );
  };

  /** Delete everything selected, after one confirmation naming all of it. */
  const removeSelected = (targets: Entry[]) => {
    if (targets.length === 0) return;
    const what =
      targets.length === 1
        ? targets[0].is_dir
          ? `Delete the folder "${targets[0].name}" and everything in it?`
          : `Delete "${targets[0].name}"?`
        : `Delete these ${targets.length} items?\n\n${targets
            .map((e) => (e.is_dir ? `${e.name}/ (and everything in it)` : e.name))
            .join("\n")}`;
    if (!window.confirm(`${what}\n\nThis cannot be undone.`)) return;
    void act("Could not delete it.", async () => {
      for (const entry of targets) {
        await api.deleteFile(store, { path: entry.path });
      }
    });
  };

  const download = (entries: Entry[]) => {
    setError(null);
    // One link per file: the browser fetches and saves each itself, at any
    // size, rather than this page holding the bytes (an APK is 100 MB).
    for (const entry of entries.filter((e) => !e.is_dir)) {
      const link = document.createElement("a");
      link.href = downloadUrl(store, entry.path);
      link.download = entry.name;
      link.click();
    }
  };

  const openEditor = async (entry: Entry) => {
    setError(null);
    try {
      const file = await api.readFile(store, { path: entry.path });
      if (file.text == null) {
        setError(`"${entry.name}" is not a text file, so it cannot be edited here.`);
        return;
      }
      setEditing({ path: entry.path, text: file.text });
    } catch (err) {
      setError(errorMessage(err, "Could not open the file."));
    }
  };

  const saveEditor = async () => {
    if (!editing) return;
    await act("Could not save the file.", () =>
      api.writeFile(store, { path: editing.path, text: editing.text, base64: null }),
    );
    setEditing(null);
  };

  const openPermissions = async (entry: Entry) => {
    setError(null);
    try {
      setPermissions(await api.getFileMeta(store, { path: entry.path }));
    } catch (err) {
      setError(errorMessage(err, "Could not read the file's permissions."));
    }
  };

  const crumbs = dir ? dir.split("/") : [];
  const columns = 6;

  return (
    <>
      <PageHeader
        pretitle={t("Data")}
        title={
          <>
            <T text="Files" /> <span className="text-muted">— {store}</span>
          </>
        }
        actions={
          <>
            <Button variant="outline-secondary" onClick={() => navigate("/file-stores")}>
              <T text="File stores" />
            </Button>
            {/* The same store, opened as a project instead of a folder of files
                (§12.1). One file at a time and a textarea is the wrong
                instrument for a source tree, and this is where the admin goes
                when that becomes obvious. */}
            <Button variant="outline-primary" href={ideUrl(store)}>
              <T text="Edit code" />
            </Button>
            <Button variant="outline-primary" onClick={makeFolder}>
              <T text="New folder" />
            </Button>
            <Button onClick={() => uploadInput.current?.click()}><T text="Upload" /></Button>
            <input
              ref={uploadInput}
              type="file"
              multiple
              className="d-none"
              onChange={(e) => {
                upload(e.target.files);
                e.target.value = "";
              }}
            />
          </>
        }
      />
      <PageBody>
        {error && (
          <Alert variant="danger" onClose={() => setError(null)} dismissible>
            {error}
          </Alert>
        )}

        <div className="d-flex flex-wrap align-items-center gap-3 mb-3">
          <Breadcrumb className="mb-0">
            <Breadcrumb.Item active={dir === ""} onClick={() => void load("")}>
              {store}
            </Breadcrumb.Item>
            {crumbs.map((name, index) => {
              const target = crumbs.slice(0, index + 1).join("/");
              return (
                <Breadcrumb.Item
                  key={target}
                  active={index === crumbs.length - 1}
                  onClick={() => void load(target)}
                >
                  {name}
                </Breadcrumb.Item>
              );
            })}
          </Breadcrumb>
          <div className="ms-auto file-search">
            <span className="file-search-icon text-muted">
              <IconSearch className="icon-2" />
            </span>
            <Form.Control
              type="search"
              className="file-search-input"
              value={search}
              placeholder={dir ? `Search in ${dir}` : `Search ${store}`}
              aria-label={t("Search files by name")}
              onChange={(e) => setSearch(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Escape") setSearch("");
              }}
            />
          </div>
        </div>

        {/* The badge the selection puts above the listing, carrying the same menu
            a row carries — so an operation on four files is the operation on one,
            in the same place.

            It is always in the layout and merely *hidden* when nothing is
            selected, rather than absent: appearing on the first click would push
            every row down by its own height, out from under the cursor that had
            just clicked one — so the next click lands on the wrong file.
            `visibility` also takes it out of the tab order while it is hidden,
            which `opacity` would not. */}
        <div
          className={`d-flex align-items-center gap-2 mb-2 file-selection-bar${
            selected.length === 0 ? " file-selection-bar-empty" : ""
          }`}
          aria-hidden={selected.length === 0}
        >
          <StatusBadge tone="blue">
            {selectionSummary(entries ?? [], selection.selected)}
          </StatusBadge>
          {/* This menu sits at the start of the bar, so it opens towards the
              listing: opening it towards the edge, as a row's menu does, would
              put it under the sidebar. */}
          <EntryMenu
            id="selection-menu"
            align="start"
            label={`Actions for ${selected.length} selected`}
            entries={selected}
            busy={busy}
            onOpen={openEntry}
            onEdit={(entry) => void openEditor(entry)}
            onDownload={() => download(selected)}
            onRename={rename}
            onPermissions={(entry) => void openPermissions(entry)}
            onDelete={() => removeSelected(selected)}
          />
          <Button
            size="sm"
            variant="link"
            className="text-muted"
            onClick={() => setSelection(NOTHING_SELECTED)}
          >
            <T text="Clear" />
          </Button>
        </div>

        {searched && (
          <div className="text-muted small mb-2">
            {entries?.length === 0
              ? `Nothing under ${dir || store} is named like "${searched.query}".`
              : `${entries?.length} match${entries?.length === 1 ? "" : "es"} for ` +
                `"${searched.query}" under ${dir || store}.`}
            {/* A ceiling was reached, so what is on screen is not all of it —
                which a result count on its own would be claiming. */}
            {searched.truncated &&
              " Showing the first results only — narrow the search to see more."}
          </div>
        )}

        {!entries ? (
          <div className="text-center py-5">
            <Spinner animation="border" role="status" />
          </div>
        ) : (
          <div className="card">
            <Table hover responsive className="card-table table-vcenter file-table">
              <thead>
                <tr>
                  <th><T text="Name" /></th>
                  <th><T text="Owner" /></th>
                  <th><T text="Modified" /></th>
                  <th className="text-end"><T text="Size" /></th>
                  <th><T text="Access" /></th>
                  <th className="w-1"></th>
                </tr>
              </thead>
              <tbody>
                {dir !== "" && !searched && (
                  <tr>
                    <td colSpan={columns}>
                      <Button
                        variant="link"
                        className="p-0"
                        onClick={() => void load(parentOf(dir))}
                      >
                        ../
                      </Button>
                    </td>
                  </tr>
                )}
                {entries.length === 0 && !searched && (
                  <tr>
                    <td colSpan={columns} className="text-muted">
                      <T text="This folder is empty." />
                    </td>
                  </tr>
                )}
                {entries.map((entry) => (
                  <tr
                    key={entry.path}
                    className={selection.selected.includes(entry.path) ? "table-active" : ""}
                    aria-selected={selection.selected.includes(entry.path)}
                    onClick={(e) => clickRow(entry, e)}
                    onDoubleClick={() => openEntry(entry)}
                  >
                    <td>
                      <div className="d-flex align-items-center gap-2">
                        <span className="text-muted">
                          {entry.is_dir ? (
                            <IconFolder className="icon-2" />
                          ) : (
                            <IconFile className="icon-2" />
                          )}
                        </span>
                        <div>
                          <div>{entry.is_dir ? `${entry.name}/` : entry.name}</div>
                          {/* Where a hit came from. A search crosses directories,
                              so the name on its own does not say which file it is. */}
                          {searched && parentDir(entry.path) && (
                            <div className="text-muted small">{parentDir(entry.path)}</div>
                          )}
                        </div>
                      </div>
                    </td>
                    <td className="text-muted small">{entry.owner ?? "—"}</td>
                    <td className="text-muted small">{formatModified(entry.modified)}</td>
                    <td className="text-muted small text-end">{formatSize(entry.size)}</td>
                    <td className="small">
                      <AccessCell entry={entry} roles={roles} />
                    </td>
                    <td className="text-end">
                      <EntryMenu
                        id={`file-menu-${entry.path}`}
                        align="end"
                        label={`Actions for ${entry.name}`}
                        entries={[entry]}
                        busy={busy}
                        onOpen={openEntry}
                        onEdit={(e) => void openEditor(e)}
                        onDownload={() => download([entry])}
                        onRename={rename}
                        onPermissions={(e) => void openPermissions(e)}
                        onDelete={() => removeSelected([entry])}
                      />
                    </td>
                  </tr>
                ))}
              </tbody>
            </Table>
          </div>
        )}

        <EditorModal
          editing={editing}
          busy={busy}
          onChange={(text) => setEditing((e) => (e ? { ...e, text } : e))}
          onCancel={() => setEditing(null)}
          onSave={() => void saveEditor()}
        />

        <PermissionsModal
          store={store}
          meta={permissions}
          onClose={() => setPermissions(null)}
          onSaved={(saved) => {
            setPermissions(null);
            void refresh();
            return saved;
          }}
          onError={setError}
        />
      </PageBody>
    </>
  );
}

/** The rule that actually reaches an entry, and whether it was set here.
 *
 * The effective role is the one shown, because it is the one that decides: a
 * file marked "unrestricted" inside an admin-only folder is not reachable, and a
 * column reporting its own rule would say it is. An inherited rule is muted and
 * says so, so an admin can see at a glance which folder in a tree carries the
 * restriction everything under it has. */
function AccessCell({ entry, roles }: { entry: Entry; roles: Roles }) {
  const effective = entry.effective_min_role ?? null;
  if (effective == null) return <span className="text-muted"><T text="Unrestricted" /></span>;
  const own = entry.min_role != null;
  return (
    <span className={own ? undefined : "text-muted"} title={own ? "Set here" : "Inherited"}>
      {roleLabel(effective, roles)}
      {!own && " (inherited)"}
    </span>
  );
}

/** The three-dots menu, over one entry or over the whole selection.
 *
 * It is one component for both because the two must not drift: an operation that
 * appears on a row and not in the selection's menu is an operation an admin
 * cannot do to four files, and finding that out costs them four trips through
 * the row menu. What differs is only what a menu item *means* for several
 * entries — open, edit, rename and permissions want exactly one, so they are
 * shown only when there is one. */
function EntryMenu({
  id,
  align,
  label,
  entries,
  busy,
  onOpen,
  onEdit,
  onDownload,
  onRename,
  onPermissions,
  onDelete,
}: {
  id: string;
  /** Which edge of the toggle the menu lines up with — `end` opens it towards
   * the start of the line, `start` towards the end. */
  align: "start" | "end";
  label: string;
  entries: Entry[];
  busy: boolean;
  onOpen: (entry: Entry) => void;
  onEdit: (entry: Entry) => void;
  onDownload: () => void;
  onRename: (entry: Entry) => void;
  onPermissions: (entry: Entry) => void;
  onDelete: () => void;
}) {
  const one = entries.length === 1 ? entries[0] : null;
  const files = entries.filter((e) => !e.is_dir);
  return (
    // The click that opens the menu must not also change the selection under it:
    // ticking four files and then reaching for their menu would select the row
    // the menu is on and throw the other three away.
    <Dropdown align={align} onClick={(e) => e.stopPropagation()}>
      <Dropdown.Toggle
        variant="outline-secondary"
        size="sm"
        className="btn-icon"
        id={id}
        disabled={busy}
        aria-label={label}
      >
        <IconDots className="icon-2" />
      </Dropdown.Toggle>
      {/* Positioned against the viewport, not the table: the listing scrolls
          sideways on a narrow screen, and a menu laid out inside that box is
          clipped by it. */}
      <Dropdown.Menu renderOnMount popperConfig={{ strategy: "fixed" }}>
        {one && (
          <Dropdown.Item onClick={() => onOpen(one)}>
            {one.is_dir ? "Open folder" : "Open"}
          </Dropdown.Item>
        )}
        {one && !one.is_dir && <Dropdown.Item onClick={() => onEdit(one)}><T text="Edit text" /></Dropdown.Item>}
        {files.length > 0 && (
          <Dropdown.Item onClick={onDownload}>
            {files.length === 1 ? "Download" : `Download ${files.length} files`}
          </Dropdown.Item>
        )}
        {/* The name on screen cannot be selected — a drag on a row is a click on
            it — so this is how a path gets out of the listing. */}
        {entries.length > 0 && (
          <Dropdown.Item onClick={() => void copyPaths(entries)}>
            {one ? <T text="Copy relative path" /> : <T text="Copy relative paths" />}
          </Dropdown.Item>
        )}
        {one && <Dropdown.Item onClick={() => onRename(one)}><T text="Rename" /></Dropdown.Item>}
        {one && (
          <Dropdown.Item onClick={() => onPermissions(one)}><T text="Permissions" /></Dropdown.Item>
        )}
        <Dropdown.Divider />
        <Dropdown.Item className="text-danger" onClick={onDelete}>
          {entries.length > 1 ? `Delete ${entries.length} items` : "Delete"}
        </Dropdown.Item>
      </Dropdown.Menu>
    </Dropdown>
  );
}

/** Put the entries' store-relative paths on the clipboard. */
async function copyPaths(entries: Entry[]) {
  try {
    await navigator.clipboard.writeText(copyablePaths(entries));
  } catch {
    // A clipboard the browser will not hand over (an insecure origin, a denied
    // permission) is not worth a banner: nothing else about the screen changed.
  }
}

/** Edit a text file in place — what `readFile`'s UTF-8 shortcut was built for. */
function EditorModal({
  editing,
  busy,
  onChange,
  onCancel,
  onSave,
}: {
  editing: { path: string; text: string } | null;
  busy: boolean;
  onChange: (text: string) => void;
  onCancel: () => void;
  onSave: () => void;
}) {
  return (
    <Modal show={editing !== null} onHide={onCancel} size="lg">
      <Modal.Header closeButton>
        <Modal.Title className="h6">{editing?.path}</Modal.Title>
      </Modal.Header>
      <Modal.Body>
        <Form.Control
          as="textarea"
          rows={20}
          className="font-monospace"
          value={editing?.text ?? ""}
          onChange={(e) => onChange(e.target.value)}
        />
      </Modal.Body>
      <Modal.Footer>
        <Button variant="outline-secondary" onClick={onCancel}>
          <T text="Cancel" />
        </Button>
        <Button onClick={onSave} disabled={busy}>
          {busy ? "Saving…" : "Save"}
        </Button>
      </Modal.Footer>
    </Modal>
  );
}

/** View and set one entry's access rule.
 *
 * Shows the *effective* role beside the one set here, because they differ
 * whenever a parent directory or the store itself is stricter — and an admin who
 * saw only the entry's own rule could easily believe a file is reachable when its
 * folder has locked it. */
function PermissionsModal({
  store,
  meta,
  onClose,
  onSaved,
  onError,
}: {
  store: string;
  meta: GetFileMetaResponse | null;
  onClose: () => void;
  onSaved: (saved: GetFileMetaResponse) => void;
  onError: (message: string) => void;
}) {
  const { t } = useT();
  const roles = useRoles();
  const [value, setValue] = useState<number | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    setValue(meta?.min_role ?? null);
  }, [meta]);

  if (!meta) return null;

  const save = async () => {
    setBusy(true);
    try {
      const saved = await api.setFileMeta(store, {
        path: meta.path,
        min_role: value,
        // Attributes are round-tripped untouched: this dialog edits the access
        // rule, and silently dropping the free-form attributes stored beside it
        // would be a destructive side effect of opening it.
        attributes: meta.attributes ?? {},
      });
      onSaved(saved);
    } catch (err) {
      onError(errorMessage(err, "Could not save the permissions."));
    } finally {
      setBusy(false);
    }
  };

  const inherited =
    meta.effective_min_role != null && meta.effective_min_role !== meta.min_role;

  return (
    <Modal show onHide={onClose}>
      <Modal.Header closeButton>
        <Modal.Title className="h6">
          {t("Permissions — {path}", { path: meta.path })}
        </Modal.Title>
      </Modal.Header>
      <Modal.Body>
        <OptionalRoleSelect
          id="metaMinRole"
          label={t("Minimum role")}
          value={value}
          roles={roles}
          blank="Unrestricted"
          onChange={setValue}
        >
          <T text="The least privileged role still allowed. Unrestricted sets no rule here." />
        </OptionalRoleSelect>

        <div className="mb-0">
          <span className="me-2">
            <T text="Effective:" />
          </span>
          {meta.effective_min_role == null ? (
            <StatusBadge tone="secondary"><T text="Unrestricted" /></StatusBadge>
          ) : (
            <StatusBadge tone="blue">
              {t("{role} or lower", {
                role: roleLabel(meta.effective_min_role, roles),
              })}
            </StatusBadge>
          )}
          {inherited && (
            <div className="text-muted small mt-2">
              <T text="Stricter than the rule set here, because a parent folder or the store itself restricts it. Access is cumulative down the path, so a rule here can only ever tighten it further." />
            </div>
          )}
        </div>
      </Modal.Body>
      <Modal.Footer>
        <Button variant="outline-secondary" onClick={onClose}>
          <T text="Cancel" />
        </Button>
        <Button onClick={() => void save()} disabled={busy}>
          {busy ? "Saving…" : "Save"}
        </Button>
      </Modal.Footer>
    </Modal>
  );
}
