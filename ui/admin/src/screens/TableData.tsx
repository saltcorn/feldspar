// Table data: a table's rows, as a grid, and the form that opens one of them.
//
// A screen of its own rather than a panel on the table page, which is the split
// Saltcorn 1 makes and for the same reason: the table page is about the table —
// its columns, its access rules, what fires on it — and is a page an admin reads
// top to bottom, while this is a grid they scroll and type in.
//
// The grid itself is `DataGrid`. What is left here is the screen around it: the
// header, what the table allows to be written to it (§8.3), and the **row form**
// — the one place a row is edited as a form rather than as cells. Two things
// need that form and cannot have cells: creating a row, which has to offer every
// column at once including the required ones the grid cannot leave blank, and a
// `File` field, whose value is a path *within* a store and so is browsed rather
// than typed. Everything else happens in the grid.

import { useCallback, useEffect, useMemo, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { DataGrid, fileStoreOf, isCalc, isDocument, isSecret, type FieldInfo } from "./DataGrid";
import { display, type RowRecord } from "../grid/gridValues";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader } from "../layout";
import { ALL_WRITES, canSubmit, formOffered, writesOf, type TableWrites } from "../tableWrites";
import type { BrowseFilesResponse, ListFieldsResponse, ListTablesResponse } from "../client";
import { T } from "../i18n";

/**
 * Best-effort parse of a form input into JSON: `5` → number, `true` → boolean,
 * plain text stays a string. The server coerces to each column's type, so this
 * only needs to turn obvious scalars into their JSON form.
 *
 * The grid's own `parseCell` is the typed sibling of this — it knows the column
 * and so can be exact. The form does not need to be: it offers every column at
 * once, including ones whose type it has no opinion about.
 */
function parseInput(raw: string): unknown {
  const trimmed = raw.trim();
  if (trimmed === "") return null;
  try {
    return JSON.parse(trimmed) as unknown;
  } catch {
    return raw;
  }
}

export function TableData({ table }: { table: string }) {
  const [fields, setFields] = useState<ListFieldsResponse | null>(null);
  const [label, setLabel] = useState<string>(table);
  // What may be written to it: all three for a table in a database, and whatever
  // its module answered for a provided one (§8.3).
  const [writes, setWrites] = useState<TableWrites>(ALL_WRITES);
  const [provider, setProvider] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  // The row the form is open on: an existing row, `null` for a new one, and
  // `undefined` when the form is closed.
  const [formRow, setFormRow] = useState<RowRecord | null | undefined>(undefined);
  // Bumped when the form wrote something, so the grid re-reads what it holds.
  const [reloadToken, setReloadToken] = useState(0);

  useEffect(() => {
    let live = true;
    setError(null);
    Promise.all([api.listFields(table), api.listTables()])
      .then(([f, t]) => {
        if (!live) return;
        setFields(f);
        const summary = (t as ListTablesResponse).find((c) => c.name === table);
        setLabel(summary?.label || table);
        setWrites(writesOf(summary?.provider));
        setProvider(summary?.provider ? summary.provider.provider : null);
      })
      .catch(() => {
        if (live) setError("Could not load the table.");
      });
    return () => {
      live = false;
    };
  }, [table]);

  const openRow = useCallback((row: RowRecord | null) => setFormRow(row), []);

  return (
    <>
      <PageHeader
        pretitle="Table data"
        title={label}
        actions={
          <Button
            variant="outline-secondary"
            onClick={() => navigate(`/tables/${encodeURIComponent(table)}`)}
          >
            <IconArrowLeft className="icon-2" />
            {label}
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {provider !== null && !formOffered(writes) && !writes.delete && (
          <Alert variant="secondary">
            <T text="These rows come from the table provider" /> <strong>{provider}</strong><T text=", which is read-only for the settings this table has. Change them on the table page if the provider can be configured to write." />
          </Alert>
        )}
        {fields !== null && fields.length === 0 && (
          <Alert variant="secondary"><T text="Add a field before creating rows." /></Alert>
        )}
        {fields !== null && fields.length > 0 && (
          <DataGrid
            table={table}
            fields={fields}
            writes={writes}
            onOpenRow={openRow}
            reloadToken={reloadToken}
          />
        )}
      </PageBody>

      {formRow !== undefined && fields !== null && (
        <RowForm
          table={table}
          fields={fields}
          writes={writes}
          row={formRow}
          onClose={() => setFormRow(undefined)}
          onSaved={() => {
            setFormRow(undefined);
            setReloadToken((t) => t + 1);
          }}
        />
      )}
    </>
  );
}

/**
 * One row as a form: every column at once, with a File field browsed rather than
 * typed.
 *
 * Opened two ways, and the difference is `row`: `null` creates, an existing row
 * updates the one it names. A calculated field is left out of both — it is
 * computed on read and refused on write — and so is a collaborative document,
 * which is edited only through its stream — and the key is shown but not editable
 * while updating, because it is what identifies the row being changed.
 */
function RowForm({
  table,
  fields,
  writes,
  row,
  onClose,
  onSaved,
}: {
  table: string;
  fields: FieldInfo[];
  writes: TableWrites;
  row: RowRecord | null;
  onClose: () => void;
  onSaved: () => void;
}) {
  const editable = useMemo(
    () => fields.filter((f) => !isCalc(f) && !isDocument(f)),
    [fields],
  );
  const pk = useMemo(() => {
    const keys = fields.filter((f) => f.primary_key);
    return keys.length === 1 ? keys[0].name : null;
  }, [fields]);

  const [values, setValues] = useState<Record<string, string>>(() => {
    const initial: Record<string, string> = {};
    // A secret's box opens empty: what the row holds is its sentinel, not its value.
    if (row !== null)
      for (const f of editable) initial[f.name] = isSecret(f) ? "" : display(row[f.name]);
    return initial;
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const editingId = row === null || pk === null ? null : display(row[pk]);
  const submittable = canSubmit(writes, editingId !== null);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    const body: RowRecord = {};
    for (const f of editable) {
      // An empty secret box means "keep what is there" (or nothing, on a new row).
      if (isSecret(f) && (values[f.name] ?? "") === "") continue;
      const value = parseInput(values[f.name] ?? "");
      // A blank box on a column that fills itself in means "let it" — an identity
      // key numbers itself, a UUID key generates itself — and sending an explicit
      // null instead would be refused by the NOT NULL every key column has.
      // Whether it does is read off the column rather than assumed from the key,
      // because a key of any other type is one somebody types.
      if (f.generated && value === null) continue;
      body[f.name] = value;
    }
    try {
      if (editingId !== null) await api.updateRow(table, editingId, body);
      else await api.createRow(table, body);
      onSaved();
    } catch (err) {
      setError(
        errorMessage(err, editingId !== null ? "Could not update the row." : "Could not create the row."),
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal show onHide={onClose} size="lg" scrollable>
      <Modal.Header closeButton>
        <Modal.Title className="h5">{editingId !== null ? "Edit row" : "New row"}</Modal.Title>
      </Modal.Header>
      <Form onSubmit={submit}>
        <Modal.Body>
          {error && <Alert variant="danger">{error}</Alert>}
          {editable.map((f) => {
            const store = fileStoreOf(f);
            return (
              <Form.Group className="mb-2" controlId={`row-${f.name}`} key={f.name}>
                <Form.Label>
                  {f.name} <span className="text-muted small">{f.type}</span>
                </Form.Label>
                {store !== null ? (
                  // A File field is a path in the field's store, so it is picked
                  // from that store rather than typed — the same browse endpoints
                  // the file manager uses.
                  <FileFieldInput
                    store={store}
                    value={values[f.name] ?? ""}
                    onChange={(v) => setValues({ ...values, [f.name]: v })}
                  />
                ) : isSecret(f) ? (
                  <Form.Control
                    type="password"
                    autoComplete="new-password"
                    value={values[f.name] ?? ""}
                    placeholder={
                      row !== null && row[f.name] !== null && row[f.name] !== undefined
                        ? "set — leave blank to keep it"
                        : "not set"
                    }
                    onChange={(e) => setValues({ ...values, [f.name]: e.target.value })}
                  />
                ) : (
                  <Form.Control
                    value={values[f.name] ?? ""}
                    // The key addresses the row being edited; changing it here
                    // would mean "move this row to another key", which the form
                    // is not for.
                    disabled={f.name === pk && editingId !== null}
                    placeholder={f.generated ? "assigned by the database if left blank" : ""}
                    onChange={(e) => setValues({ ...values, [f.name]: e.target.value })}
                  />
                )}
              </Form.Group>
            );
          })}
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" onClick={onClose} type="button">
            <T text="Cancel" />
          </Button>
          <Button type="submit" disabled={busy || !submittable}>
            {editingId !== null ? "Save changes" : "Add row"}
          </Button>
        </Modal.Footer>
      </Form>
    </Modal>
  );
}

/**
 * A row input for a `File` field: the chosen path, plus a "Choose" button that
 * opens a browser over the field's store (design §3.4).
 *
 * A File field is a path *within* a store, so a free-text box would let a typo
 * point it anywhere; browsing the store instead means every value is a path that
 * exists in the store the field is bound to. It reuses `browseFiles` — the same
 * endpoint the file manager is built on — rather than a new surface.
 */
function FileFieldInput({
  store,
  value,
  onChange,
}: {
  store: string;
  value: string;
  onChange: (value: string) => void;
}) {
  const [show, setShow] = useState(false);
  const [dir, setDir] = useState("");
  const [entries, setEntries] = useState<BrowseFilesResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (!show) return;
    let cancelled = false;
    setError(null);
    setEntries(null);
    api
      .browseFiles(store, { dir })
      .then((list) => {
        if (!cancelled) setEntries(list);
      })
      .catch(() => {
        if (!cancelled) setError(`Could not browse the store “${store}”.`);
      });
    return () => {
      cancelled = true;
    };
  }, [show, dir, store]);

  const open = () => {
    setDir("");
    setShow(true);
  };

  const parent = dir.includes("/") ? dir.slice(0, dir.lastIndexOf("/")) : "";

  return (
    <>
      <div className="d-flex gap-2">
        <Form.Control
          value={value}
          placeholder={`path in ${store}`}
          onChange={(e) => onChange(e.target.value)}
        />
        <Button size="sm" variant="outline-secondary" type="button" onClick={open}>
          <T text="Choose…" />
        </Button>
      </div>

      <Modal show={show} onHide={() => setShow(false)}>
        <Modal.Header closeButton>
          <Modal.Title className="h6">
            {store}
            {dir && ` / ${dir}`}
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          {error && <Alert variant="danger">{error}</Alert>}
          {!entries && !error && <p className="text-muted mb-0"><T text="Loading…" /></p>}
          {entries && (
            <div className="list-group">
              {dir !== "" && (
                <button
                  type="button"
                  className="list-group-item list-group-item-action"
                  onClick={() => setDir(parent)}
                >
                  <T text="← up" />
                </button>
              )}
              {entries.length === 0 && <div className="list-group-item text-muted">Empty.</div>}
              {entries.map((entry) => (
                <button
                  key={entry.path}
                  type="button"
                  className="list-group-item list-group-item-action d-flex justify-content-between"
                  onClick={() => {
                    if (entry.is_dir) {
                      setDir(entry.path);
                    } else {
                      onChange(entry.path);
                      setShow(false);
                    }
                  }}
                >
                  <span>
                    {entry.is_dir ? "📁 " : "📄 "}
                    {entry.name}
                  </span>
                  {!entry.is_dir && entry.size != null && (
                    <span className="text-muted small">{entry.size} B</span>
                  )}
                </button>
              ))}
            </div>
          )}
        </Modal.Body>
      </Modal>
    </>
  );
}
