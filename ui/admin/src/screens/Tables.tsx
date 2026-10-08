// Tables list: shows every table in the catalog and creates new ones. Each row
// links to the table detail screen (settings + fields + row editor).
//
// Creating is behind a dialog rather than an input beside the button, because
// there is more than one way to make a table: empty, or from a CSV file whose
// header and contents decide the fields (§13.1). A name box that could only make
// the first kind would have to grow a second control anyway the moment the
// second existed, and the two would be asking for the same name twice. What the
// dialog *knows* — when Create may be pressed, what a chosen file suggests the
// table be called — is in `newTable.ts`, where it can be tested without a
// browser.
//
// The read/write roles are in this list, not only on the detail screen, because
// "which of these can the public read?" is a question about the whole set. The
// orphan banner is the visible half of a storage decision: settings for a table
// that is not in the database are kept rather than deleted (design §9), so that
// a dropped-and-recreated table gets its rules back — and something has to say
// they are there, or "kept" would mean "invisible".

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type {
  ListDatabaseConnectionsResponse,
  ListOrphanTableSettingsResponse,
  ListTableProvidersResponse,
  ListTablesResponse,
} from "../client";
import { navigate } from "../App";
import { AlertBody, PageBody, PageHeader, StatusBadge } from "../layout";
import {
  EMPTY_NEW_TABLE_FORM,
  PRIMARY_DATABASE,
  creatableDatabases,
  databaseLabel,
  GEO_FILE_ACCEPT,
  importedMessage,
  newTableError,
  providerKey,
  providerLabel,
  splitProviderKey,
  tableNameFromFile,
  toBase64,
  type NewTableForm,
} from "../newTable";
import { roleLabel, useRoles } from "../roles";
import { SettingsFields, buildConfig, initialValues } from "../settings";
import { T, useT } from "../i18n";

export function Tables() {
  const { t } = useT();
  const [tables, setTables] = useState<ListTablesResponse | null>(null);
  const [orphans, setOrphans] = useState<ListOrphanTableSettingsResponse>([]);
  const [connections, setConnections] = useState<ListDatabaseConnectionsResponse>([]);
  const [providers, setProviders] = useState<ListTableProvidersResponse>([]);
  const [metadataTables, setMetadataTables] = useState<string[]>([]);
  const [creating, setCreating] = useState<NewTableForm | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const roles = useRoles();

  const load = async () => {
    try {
      const [t, o] = await Promise.all([api.listTables(), api.listOrphanTableSettings()]);
      setTables(t);
      setOrphans(o);
    } catch {
      setError("Could not load tables.");
    }
    // The connections come along because the New table dialog needs them, and
    // separately because they are not what this screen is *for*: an
    // installation with none is the common one, and a failure here must leave
    // the tables list working with the chooser absent rather than blanking the
    // page it is a detail of.
    try {
      setConnections(await api.listDatabaseConnections());
    } catch {
      setConnections([]);
    }
    // The table providers, for the same reason and on the same terms: a server
    // with no modules supplies none, the dialog then offers only the two kinds
    // it always had, and a failure here must not blank the list.
    try {
      setProviders(await api.listTableProviders());
    } catch {
      setProviders([]);
    }
    // The metadata tables not yet on the list, on the same terms again.
    try {
      setMetadataTables(await api.listMetadataTables());
    } catch {
      setMetadataTables([]);
    }
  };

  const forget = async (table: string) => {
    setBusy(true);
    try {
      await api.deleteTableSettings(table);
      await load();
    } catch {
      setError("Could not forget those settings.");
    } finally {
      setBusy(false);
    }
  };

  useEffect(() => {
    void load();
  }, []);

  /**
   * Make the table the dialog describes.
   *
   * The CSV path reads the file in the browser and sends it as text: the
   * endpoint model is JSON and a CSV is text, so there is no upload route to go
   * through (the same reason `importTableCsv` takes a string). The server's own
   * refusal is shown as it came — "`!` cannot be a column name", "the rows could
   * not be imported" — because it names the thing to fix in the file, which
   * nothing here could guess.
   */
  const create = async (e: FormEvent) => {
    e.preventDefault();
    if (!creating || newTableError(creating)) return;
    const name = creating.name.trim();
    setBusy(true);
    setError(null);
    setNotice(null);
    try {
      const database = creating.database;
      const chosen = splitProviderKey(creating.provider);
      if (creating.source === "metadata") {
        // Nothing is created: the table is Saltcorn's and already there, and
        // this puts it on the list.
        await api.createMetadataTable({ name: creating.metadataTable });
      } else if (creating.source === "provider" && chosen) {
        // No database and no DDL: this writes the table's definition, and the
        // module is asked for its columns as part of creating it.
        await api.createProvidedTable({
          name,
          module: chosen.module,
          provider: chosen.provider,
          configuration: buildConfig(
            providers.find(
              (p) => p.module === chosen.module && p.provider === chosen.provider,
            )?.config_spec ?? [],
            creating.providerConfig,
          ),
        });
      } else if (creating.source === "csv" && creating.file) {
        const csv = await creating.file.text();
        const { table, inserted } = await api.createTableFromCsv({ name, csv, database });
        setNotice(importedMessage(table.name, inserted));
      } else if (creating.source === "geo" && creating.file) {
        // Two of the three formats are binary, so the file goes as base64.
        const bytes = new Uint8Array(await creating.file.arrayBuffer());
        const { table, inserted, warnings } = await api.createTableFromGeoFile({
          name,
          file_name: creating.file.name,
          content_base64: toBase64(bytes),
          layer: creating.layer.trim() || null,
          database,
        });
        setNotice([importedMessage(table.name, inserted), ...warnings].join(" "));
      } else {
        await api.createTable({ name, database });
      }
      setCreating(null);
      await load();
    } catch (err) {
      setError(errorMessage(err, "Could not create the table."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <PageHeader
        pretitle="Data"
        title={t("Tables")}
        actions={
          <>
            <Button variant="outline-secondary" onClick={() => navigate("/db-connections")}>
              <T text="Connections" />
            </Button>
            <Button onClick={() => setCreating({ ...EMPTY_NEW_TABLE_FORM })}><T text="+ New table" /></Button>
          </>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {notice && (
          <Alert variant="success" dismissible onClose={() => setNotice(null)}>
            {notice}
          </Alert>
        )}

        {orphans.length > 0 && (
          <Alert variant="warning">
            <AlertBody>
              <Alert.Heading className="h6"><T text="Settings without a table" /></Alert.Heading>
              <p className="mb-2">
                <T text="These stored settings name tables that are not in the database. They are kept in case the table comes back — recreating it restores its access rules — but nothing is using them right now." />
              </p>
              <ul className="mb-0 list-unstyled">
                {orphans.map((o) => (
                  <li key={o.name} className="d-flex align-items-center gap-2 mb-1">
                    <code>{o.name}</code>
                    <span className="text-muted small">
                      {t("read {read}, write {write}", {
                        read: roleLabel(o.min_role_read, roles),
                        write: roleLabel(o.min_role_write, roles),
                      })}
                    </span>
                    <Button
                      size="sm"
                      variant="outline-secondary"
                      disabled={busy}
                      onClick={() => void forget(o.name)}
                    >
                      <T text="Forget" />
                    </Button>
                  </li>
                ))}
              </ul>
            </AlertBody>
          </Alert>
        )}

        <div className="card">
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Name" /></th>
                <th><T text="Read" /></th>
                <th><T text="Write" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {tables?.length === 0 && (
                <tr>
                  <td colSpan={4} className="text-muted">
                    <T text="No tables yet." />
                  </td>
                </tr>
              )}
              {tables?.map((table) => (
                <tr key={table.name}>
                  <td>
                    {table.label && table.label !== table.name ? (
                      <>
                        {table.label} <span className="text-muted small">({table.name})</span>
                      </>
                    ) : (
                      table.name
                    )}
                    {/* Ownership marks: the roles alone no longer tell the whole
                        access story for a table with a formula, so say so here. */}
                    {table.ownership_formula && !table.ownership_error && (
                      <StatusBadge tone="blue" className="ms-2" title={table.ownership_formula}>
                        <T text="formula" />
                      </StatusBadge>
                    )}
                    {table.ownership_error && (
                      <StatusBadge tone="yellow" className="ms-2" title={table.ownership_error}>
                        <T text="formula error" />
                      </StatusBadge>
                    )}
                    {table.rls_enabled && (
                      <StatusBadge tone="secondary" className="ms-2">
                        <T text="RLS" />
                      </StatusBadge>
                    )}
                    {table.metadata && (
                      <StatusBadge
                        tone="secondary"
                        className="ms-2"
                        title={t("One of Saltcorn's own metadata tables. Its rows and settings can be edited; its fields cannot.")}
                      >
                        <T text="metadata" />
                      </StatusBadge>
                    )}
                    {/* Which database it came from, whenever that is not
                        Saltcorn's own. Unbadged means primary — the common case,
                        and the one an installation with no connections is
                        entirely made of, so badging it would put a mark on every
                        row and tell nobody anything. */}
                    {table.database !== PRIMARY_DATABASE && (
                      <StatusBadge
                        tone="blue"
                        className="ms-2"
                        title={`This table lives in the database connection "${table.database}". Saltcorn reads and writes its rows but does not change its schema.`}
                      >
                        {table.database}
                      </StatusBadge>
                    )}
                  </td>
                  <td>{roleLabel(table.min_role_read, roles)}</td>
                  <td>{roleLabel(table.min_role_write, roles)}</td>
                  <td className="text-end">
                    <Button
                      size="sm"
                      variant="outline-primary"
                      href={`#/tables/${encodeURIComponent(table.name)}`}
                    >
                      <T text="Open" />
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        </div>
      </PageBody>

      <NewTableModal
        form={creating}
        databases={creatableDatabases(connections)}
        providers={providers}
        metadataTables={metadataTables}
        busy={busy}
        onChange={setCreating}
        onCancel={() => setCreating(null)}
        onSubmit={create}
      />
    </>
  );
}

/**
 * The New table dialog: a name, what kind, and — for a CSV — the file.
 *
 * The file input appears only for the CSV choice rather than being greyed out
 * beside it, because an input that cannot be used is a question that should not
 * have been asked. Choosing a file into an *empty* name box fills the name in
 * from the file's own name, which is what it is called nine times in ten.
 */
function NewTableModal({
  form,
  databases,
  providers,
  metadataTables,
  busy,
  onChange,
  onCancel,
  onSubmit,
}: {
  form: NewTableForm | null;
  databases: string[];
  providers: ListTableProvidersResponse;
  /** The metadata tables not yet on the list. */
  metadataTables: string[];
  busy: boolean;
  onChange: (form: NewTableForm) => void;
  onCancel: () => void;
  onSubmit: (e: FormEvent) => void;
}) {
  const { t } = useT();
  const problem = form ? newTableError(form) : null;
  const chosen = form ? splitProviderKey(form.provider) : null;
  const spec =
    providers.find((p) => p.module === chosen?.module && p.provider === chosen?.provider)
      ?.config_spec ?? [];
  return (
    <Modal show={form !== null} onHide={onCancel}>
      {form && (
        <Form onSubmit={onSubmit}>
          <Modal.Header closeButton>
            <Modal.Title className="h4"><T text="New table" /></Modal.Title>
          </Modal.Header>
          <Modal.Body>
            {/* A metadata table already has a name; a label can be given on its
                settings page. */}
            {form.source !== "metadata" && (
              <Form.Group className="mb-3" controlId="new-table-name">
                <Form.Label><T text="Name" /></Form.Label>
                <Form.Control
                  autoFocus
                  placeholder={t("e.g. invoice")}
                  value={form.name}
                  onChange={(e) => onChange({ ...form, name: e.target.value })}
                />
              </Form.Group>
            )}

            {/* Only when there is a choice. An installation with no
                connections has exactly one database, and asking which one to
                use would be a question with one answer — the same reason the
                CSV file input appears only for the CSV choice. */}
            {databases.length > 1 && form.source !== "provider" && form.source !== "metadata" && (
              <Form.Group className="mb-3" controlId="new-table-database">
                <Form.Label><T text="Database" /></Form.Label>
                <Form.Select
                  value={form.database}
                  onChange={(e) => onChange({ ...form, database: e.target.value })}
                >
                  {databases.map((name) => (
                    <option key={name} value={name}>
                      {databaseLabel(name)}
                    </option>
                  ))}
                </Form.Select>
                {form.database !== PRIMARY_DATABASE && (
                  <Form.Text className="text-muted">
                    <T text="The table is created in the" /> <code>{form.database}</code> <T text="connection’s schema, in that database — not in Saltcorn’s own." />
                  </Form.Text>
                )}
              </Form.Group>
            )}

            <Form.Group className="mb-3" controlId="new-table-source">
              <Form.Label><T text="Type" /></Form.Label>
              <Form.Select
                value={form.source}
                onChange={(e) =>
                  onChange({ ...form, source: e.target.value as NewTableForm["source"] })
                }
              >
                <option value="blank"><T text="New database table" /></option>
                <option value="csv"><T text="Create from CSV" /></option>
                <option value="geo"><T text="Create from a map file" /></option>
                {/* Offered only when a module supplies one. A chooser whose one
                    entry is "there are none" is a question with no answer, and
                    an installation with no modules is most of them. */}
                {providers.length > 0 && <option value="provider"><T text="From a table provider" /></option>}
                {/* On the same terms: offered while there is one left to add. */}
                {metadataTables.length > 0 && <option value="metadata"><T text="Metadata table" /></option>}
              </Form.Select>
            </Form.Group>

            {form.source === "metadata" && (
              <Form.Group className="mb-3" controlId="new-table-metadata">
                <Form.Label><T text="Metadata table" /></Form.Label>
                <Form.Select
                  value={form.metadataTable}
                  onChange={(e) => onChange({ ...form, metadataTable: e.target.value })}
                >
                  <option value=""><T text="Choose a metadata table…" /></option>
                  {metadataTables.map((name) => (
                    <option key={name} value={name}>
                      {name}
                    </option>
                  ))}
                </Form.Select>
                <Form.Text className="text-muted">
                  <T text="One of Saltcorn’s own tables, added to this list so its rows and settings can be edited. It starts admin-only. Its fields are Saltcorn’s and cannot be changed, and removing it from the list never drops it." />
                </Form.Text>
              </Form.Group>
            )}

            {form.source === "provider" && (
              <>
                <Form.Group className="mb-3" controlId="new-table-provider">
                  <Form.Label><T text="Table provider" /></Form.Label>
                  <Form.Select
                    value={form.provider}
                    onChange={(e) =>
                      // The settings belong to the provider that declared them,
                      // so changing the provider starts its own form from its own
                      // defaults rather than carrying the last one's values into
                      // fields that happen to share a name.
                      onChange({
                        ...form,
                        provider: e.target.value,
                        providerConfig: initialValues(
                          providers.find(
                            (p) => providerKey(p.module, p.provider) === e.target.value,
                          )?.config_spec ?? [],
                          {},
                        ),
                      })
                    }
                  >
                    <option value=""><T text="Choose a provider…" /></option>
                    {providers.map((p) => (
                      <option
                        key={providerKey(p.module, p.provider)}
                        value={providerKey(p.module, p.provider)}
                      >
                        {providerLabel(p.module, p.provider)}
                      </option>
                    ))}
                  </Form.Select>
                  <Form.Text className="text-muted">
                    <T text="The rows come from the module, not from a database. Saltcorn reads them; it does not create, change or delete them, and the columns are the provider’s to decide." />
                  </Form.Text>
                </Form.Group>

                {/* The provider's own settings form, declared by the module and
                    rendered by the same component every other configurable thing
                    here uses. */}
                <SettingsFields
                  spec={spec}
                  values={form.providerConfig}
                  idPrefix="new-table-provider-cfg"
                  onChange={(name, value) =>
                    onChange({
                      ...form,
                      providerConfig: { ...form.providerConfig, [name]: value },
                    })
                  }
                />
              </>
            )}

            {form.source === "geo" && (
              <>
                <Form.Group className="mb-3" controlId="new-table-geo">
                  <Form.Label><T text="Map file" /></Form.Label>
                  <Form.Control
                    type="file"
                    accept={GEO_FILE_ACCEPT}
                    onChange={(e) => {
                      const file = (e.target as HTMLInputElement).files?.[0] ?? null;
                      onChange({
                        ...form,
                        file,
                        name: form.name || (file ? tableNameFromFile(file.name) : ""),
                      });
                    }}
                  />
                  <Form.Text className="text-muted">
                    <T text="GeoJSON, a zipped Shapefile (the .shp with its .dbf and .prj) or a GeoPackage. The attributes become fields, the shapes a geometry field, reprojected to longitude and latitude. Needs PostGIS." />
                  </Form.Text>
                </Form.Group>
                <Form.Group controlId="new-table-geo-layer">
                  <Form.Label><T text="Layer (optional)" /></Form.Label>
                  <Form.Control
                    placeholder={t("only when the file holds several")}
                    value={form.layer}
                    onChange={(e) => onChange({ ...form, layer: e.target.value })}
                  />
                </Form.Group>
              </>
            )}

            {form.source === "csv" && (
              <Form.Group controlId="new-table-csv">
                <Form.Label><T text="CSV file" /></Form.Label>
                <Form.Control
                  type="file"
                  accept=".csv,text/csv"
                  onChange={(e) => {
                    const file = (e.target as HTMLInputElement).files?.[0] ?? null;
                    onChange({
                      ...form,
                      file,
                      // A name already typed is the admin's and is kept.
                      name: form.name || (file ? tableNameFromFile(file.name) : ""),
                    });
                  }}
                />
                <Form.Text className="text-muted">
                  <T text="The columns become the table’s fields — named and typed from the header and the values under it — and every row is imported. A row the file’s own columns will not take is reported and no table is created." />
                </Form.Text>
              </Form.Group>
            )}
          </Modal.Body>
          <Modal.Footer>
            {problem && <span className="text-muted small me-auto">{problem}</span>}
            <Button variant="outline-secondary" onClick={onCancel} disabled={busy}>
              <T text="Cancel" />
            </Button>
            <Button type="submit" disabled={busy || problem !== null}>
              <T text="Create" />
            </Button>
          </Modal.Footer>
        </Form>
      )}
    </Modal>
  );
}
