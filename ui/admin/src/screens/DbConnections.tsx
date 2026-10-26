// The database connections list: every *other* database whose tables share the
// tables list.
//
// The file-stores list with a different noun, and with the same two states that
// make that screen worth having: a connection whose host is down is **still
// here**, marked, with the reason beside it — because editing it is the repair,
// and a list that hid it would hide the only screen the repair happens on.
//
// The third state is the one peculiar to a database, and the reason the summary
// column exists: a connection can dial perfectly and contribute nothing, because
// the schema it names is empty or is not the one the tables are in. That looks
// identical to a working connection from the outside — same green badge, same
// row — and the only thing that tells them apart is counting what actually
// reached the catalog. So the row says how many tables it put in the list, and
// names the ones it could not (a table whose name Saltcorn's own database
// already uses; the primary always wins).
//
// Editing is a dialog rather than a screen of its own, unlike an LLM provider's
// form: there is a handful of boxes and no backend-declared settings to render.
//
// It does branch on one thing: **which kind of database**. A PostgreSQL
// connection is a server, so it is a host and a role. A SQLite connection is a
// *file*, so the dialog does not ask for a path into the server's filesystem —
// it browses the file stores, which is where Saltcorn's files already are and
// where the admin already decided who may read them.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { BrowseFilesResponse, ListDatabaseConnectionsResponse } from "../client";
import {
  EMPTY_DB_CONNECTION_FORM,
  connectionBody,
  connectionSummary,
  connectionTarget,
  dbConnectionError,
  formFromConnection,
  type DbConnectionForm,
} from "../dbConnection";
import { IconPlus } from "../icons";
import { PageBody, PageHeader, StatusBadge } from "../layout";
import { T, useT } from "../i18n";

type ConnectionItem = ListDatabaseConnectionsResponse[number];

/** The result of pressing Test: what the server said, kept beside the dialog. */
type TestResult = { connected: boolean; error?: string | null; tables: number };

export function DbConnections() {
  const { t } = useT();
  const [connections, setConnections] = useState<ConnectionItem[] | null>(null);
  const [editing, setEditing] = useState<DbConnectionForm | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = async () => {
    try {
      setConnections(await api.listDatabaseConnections());
    } catch (err) {
      setError(errorMessage(err, "Could not load the database connections."));
    }
  };

  useEffect(() => {
    void load();
  }, []);

  const save = async (form: DbConnectionForm) => {
    setBusy(true);
    setError(null);
    try {
      const body = connectionBody(form);
      if (form.id) await api.updateDatabaseConnection(form.id, body);
      else await api.createDatabaseConnection(body);
      setEditing(null);
      await load();
    } catch (err) {
      // The server names what is wrong — a name already taken, a schema that is
      // not an identifier — so surface its message rather than a generic one.
      setError(errorMessage(err, "Could not save the database connection."));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (connection: ConnectionItem) => {
    if (
      !window.confirm(
        t(
          'Remove the database connection "{name}"?\n\nIts tables leave the tables list. Nothing in that database is changed or deleted.',
          { name: connection.name },
        ),
      )
    ) {
      return;
    }
    setError(null);
    try {
      await api.deleteDatabaseConnection(connection.id);
      await load();
    } catch (err) {
      setError(errorMessage(err, "Could not remove the database connection."));
    }
  };

  return (
    <>
      <PageHeader
        pretitle="Data"
        title={t("Database connections")}
        actions={
          <Button onClick={() => setEditing({ ...EMPTY_DB_CONNECTION_FORM })}>
            <IconPlus className="icon-2" />
            <T text="New connection" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}

        <div className="card">
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Name" /></th>
                <th><T text="Connects to" /></th>
                <th><T text="State" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {connections?.length === 0 && (
                <tr>
                  <td colSpan={4} className="text-muted">
                    <T text="No external database connections." />
                  </td>
                </tr>
              )}
              {connections?.map((connection) => (
                <tr key={connection.id}>
                  <td>
                    {connection.name}
                    {connection.description && (
                      <div className="text-muted small">{connection.description}</div>
                    )}
                  </td>
                  <td className="text-break small">{connectionTarget(connection)}</td>
                  <td>
                    <StatusBadge tone={connection.connected ? "green" : "red"}>
                      {connection.connected ? "Connected" : "Not connected"}
                    </StatusBadge>
                    <div className="text-muted small">{connectionSummary(connection)}</div>
                    {connection.shadowed.length > 0 && (
                      <div className="text-muted small">
                        {t(
                          "Not listed, because Saltcorn’s own database already has a table of that name: {names}",
                          { names: connection.shadowed.join(", ") },
                        )}
                      </div>
                    )}
                  </td>
                  <td className="text-end">
                    <div className="btn-list justify-content-end flex-nowrap align-items-center">
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        onClick={() => setEditing(formFromConnection(connection))}
                      >
                        <T text="Edit" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-danger"
                        onClick={() => void remove(connection)}
                      >
                        <T text="Remove" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        </div>

      </PageBody>

      <ConnectionModal
        form={editing}
        busy={busy}
        onChange={setEditing}
        onCancel={() => setEditing(null)}
        onSubmit={save}
      />
    </>
  );
}

/**
 * The connect/edit dialog: where to dial, as whom, and which schema.
 *
 * Test is beside Save rather than behind it because the alternative is the
 * failure this whole screen is arranged against — saving a connection that
 * cannot dial, and finding out from an unchanged tables list. It sends what the
 * form currently holds and saves nothing.
 */
function ConnectionModal({
  form,
  busy,
  onChange,
  onCancel,
  onSubmit,
}: {
  form: DbConnectionForm | null;
  busy: boolean;
  onChange: (form: DbConnectionForm) => void;
  onCancel: () => void;
  onSubmit: (form: DbConnectionForm) => void;
}) {
  const { t } = useT();
  const [tested, setTested] = useState<TestResult | null>(null);
  const [testing, setTesting] = useState(false);

  // A dialog opened on a different connection must not still be showing the
  // last one's test result.
  useEffect(() => {
    setTested(null);
  }, [form?.id, form === null]);

  if (!form) return null;
  const problem = dbConnectionError(form);
  const set = (over: Partial<DbConnectionForm>) => onChange({ ...form, ...over });

  const test = async () => {
    setTesting(true);
    setTested(null);
    try {
      setTested(await api.testDatabaseConnection(connectionBody(form)));
    } catch (err) {
      setTested({
        connected: false,
        error: errorMessage(err, "Could not test the connection."),
        tables: 0,
      });
    } finally {
      setTesting(false);
    }
  };

  const submit = (e: FormEvent) => {
    e.preventDefault();
    if (!problem) onSubmit(form);
  };

  return (
    <Modal show onHide={onCancel} centered>
      <Form onSubmit={submit}>
        <Modal.Header closeButton>
          <Modal.Title>{form.id ? "Edit database connection" : "Connect a database"}</Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <Form.Group className="mb-3">
            <Form.Label><T text="Name" /></Form.Label>
            <Form.Control
              autoFocus
              value={form.name}
              onChange={(e) => set({ name: e.target.value })}
              placeholder={t("reporting")}
            />
            <Form.Text className="text-muted">
              <T text="Shown beside every table this connection brings into the tables list." />
            </Form.Text>
          </Form.Group>

          <Form.Group className="mb-3">
            <Form.Label><T text="Description" /></Form.Label>
            <Form.Control
              value={form.description}
              onChange={(e) => set({ description: e.target.value })}
            />
          </Form.Group>

          <Form.Group className="mb-3">
            <Form.Label><T text="Kind" /></Form.Label>
            <Form.Select
              value={form.backend}
              onChange={(e) =>
                set({ backend: e.target.value === "sqlite" ? "sqlite" : "postgres" })
              }
              // The kind decides what the row *is*, and changing it on a saved
              // connection would silently repoint every table stamped with its
              // name at a different database.
              disabled={!!form.id}
            >
              <option value="postgres"><T text="PostgreSQL server" /></option>
              <option value="sqlite"><T text="SQLite file" /></option>
            </Form.Select>
            <Form.Text className="text-muted">
              {form.backend === "sqlite"
                ? "A SQLite database is a file: pick it from one of the file stores."
                : "Another PostgreSQL server, reached over the network."}
            </Form.Text>
          </Form.Group>

          {form.backend === "sqlite" ? (
            <SqliteFilePicker form={form} onChange={onChange} />
          ) : (
            <>
              <div className="row">
                <Form.Group className="mb-3 col-8">
                  <Form.Label><T text="Host" /></Form.Label>
                  <Form.Control
                    value={form.host}
                    onChange={(e) => set({ host: e.target.value })}
                    placeholder="db.example.com"
                  />
                </Form.Group>
                <Form.Group className="mb-3 col-4">
                  <Form.Label><T text="Port" /></Form.Label>
                  <Form.Control
                    value={form.port}
                    onChange={(e) => set({ port: e.target.value })}
                    placeholder="5432"
                  />
                </Form.Group>
              </div>

              <Form.Group className="mb-3">
                <Form.Label><T text="Database" /></Form.Label>
                <Form.Control
                  value={form.database}
                  onChange={(e) => set({ database: e.target.value })}
                />
              </Form.Group>

              <div className="row">
                <Form.Group className="mb-3 col-6">
                  <Form.Label><T text="Username" /></Form.Label>
                  <Form.Control
                    value={form.username}
                    onChange={(e) => set({ username: e.target.value })}
                  />
                </Form.Group>
                <Form.Group className="mb-3 col-6">
                  <Form.Label><T text="Password" /></Form.Label>
                  <Form.Control
                    type="password"
                    value={form.password}
                    onChange={(e) => set({ password: e.target.value })}
                  />
                </Form.Group>
              </div>

              <Form.Group className="mb-3">
                <Form.Label><T text="Schema" /></Form.Label>
                <Form.Control
                  value={form.schema}
                  onChange={(e) => set({ schema: e.target.value })}
                  placeholder={t("public")}
                />
                <Form.Text className="text-muted">
                  <T text="One schema per connection: its tables are the ones that join the tables list." />
                </Form.Text>
              </Form.Group>
            </>
          )}

          {problem && <div className="text-muted small">{problem}</div>}
          {tested && (
            <Alert variant={tested.connected ? "success" : "danger"} className="mb-0 mt-2">
              {tested.connected
                ? `Connected. ${tested.tables} ${tested.tables === 1 ? "table" : "tables"}` +
                (form.backend === "sqlite" ? " in the file." : ` in schema ${form.schema || "public"}.`)
                : (tested.error ?? "Could not connect.")}
            </Alert>
          )}
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" onClick={onCancel} disabled={busy}>
            <T text="Cancel" />
          </Button>
          <Button
            variant="outline-secondary"
            onClick={() => void test()}
            disabled={busy || testing || !!problem}
          >
            {testing ? "Testing…" : "Test"}
          </Button>
          <Button type="submit" disabled={busy || !!problem}>
            {form.id ? "Save" : "Connect"}
          </Button>
        </Modal.Footer>
      </Form>
    </Modal>
  );
}

/**
 * Choosing the SQLite file: which store, and which file in it.
 *
 * A browser rather than a path box, and a **file store** rather than a
 * filesystem path, for two reasons. The admin does not necessarily know the
 * server's directory layout — the stores are the names they gave the places
 * files live — and a bare path would let anyone with this screen open any file
 * the server process can read. The path is still typeable, because an admin who
 * knows exactly where the file is should not have to click to it.
 *
 * Only **connected** stores are offered: a store that is defined but not
 * connected has no directory to browse and no path to resolve against, so
 * offering it would be offering a choice that can only fail.
 */
function SqliteFilePicker({
  form,
  onChange,
}: {
  form: DbConnectionForm;
  onChange: (form: DbConnectionForm) => void;
}) {
  const [stores, setStores] = useState<Array<string> | null>(null);
  const [dir, setDir] = useState("");
  const [entries, setEntries] = useState<BrowseFilesResponse | null>(null);
  const [browseError, setBrowseError] = useState<string | null>(null);
  const set = (over: Partial<DbConnectionForm>) => onChange({ ...form, ...over });

  useEffect(() => {
    void (async () => {
      try {
        const listed = await api.listFileStores();
        setStores(listed.filter((store) => store.connected).map((store) => store.name));
      } catch (err) {
        setBrowseError(errorMessage(err, "Could not load the file stores."));
      }
    })();
  }, []);

  // The directory the file is in, so opening the dialog on a saved connection
  // starts where its file is rather than at the root.
  useEffect(() => {
    const at = form.filePath.lastIndexOf("/");
    setDir(at === -1 ? "" : form.filePath.slice(0, at));
  }, [form.id]);

  useEffect(() => {
    if (!form.fileStore) {
      setEntries(null);
      return;
    }
    void (async () => {
      try {
        setBrowseError(null);
        setEntries(await api.browseFiles(form.fileStore, { dir }));
      } catch (err) {
        setEntries(null);
        setBrowseError(errorMessage(err, "Could not list that folder."));
      }
    })();
  }, [form.fileStore, dir]);

  const parent = dir.includes("/") ? dir.slice(0, dir.lastIndexOf("/")) : "";

  return (
    <>
      <Form.Group className="mb-3">
        <Form.Label><T text="File store" /></Form.Label>
        <Form.Select
          value={form.fileStore}
          onChange={(e) => {
            setDir("");
            set({ fileStore: e.target.value, filePath: "" });
          }}
        >
          <option value=""><T text="Choose a file store…" /></option>
          {stores?.map((name) => (
            <option key={name} value={name}>
              {name}
            </option>
          ))}
        </Form.Select>
        {stores?.length === 0 && (
          <Form.Text className="text-muted">
            <T text="No file store is connected. Add one under Files first — a SQLite database is a file, and this is where Saltcorn keeps files." />
          </Form.Text>
        )}
      </Form.Group>

      <Form.Group className="mb-3">
        <Form.Label><T text="File" /></Form.Label>
        <Form.Control
          value={form.filePath}
          onChange={(e) => set({ filePath: e.target.value })}
          placeholder="data/reporting.sqlite"
        />
      </Form.Group>

      {browseError && (
        <Alert variant="danger" className="py-2 small">
          {browseError}
        </Alert>
      )}

      {form.fileStore && entries && (
        <div className="mb-3 border rounded" style={{ maxHeight: "12rem", overflowY: "auto" }}>
          <div className="px-2 py-1 small text-muted border-bottom">/{dir}</div>
          {dir !== "" && (
            <button
              type="button"
              className="btn btn-link btn-sm d-block text-start w-100 text-decoration-none"
              onClick={() => setDir(parent)}
            >
              ../
            </button>
          )}
          {entries.map((entry) => (
            <button
              key={entry.path}
              type="button"
              className={`btn btn-link btn-sm d-block text-start w-100 text-decoration-none${entry.path === form.filePath ? " fw-bold" : ""
                }`}
              onClick={() =>
                entry.is_dir ? setDir(entry.path) : set({ filePath: entry.path })
              }
            >
              {entry.is_dir ? `${entry.name}/` : entry.name}
            </button>
          ))}
          {entries.length === 0 && (
            <div className="px-2 py-1 small text-muted"><T text="This folder is empty." /></div>
          )}
        </div>
      )}
    </>
  );
}
