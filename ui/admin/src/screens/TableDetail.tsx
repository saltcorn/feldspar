// Table detail: everything about one table, top to bottom.
//
// The order is the one Saltcorn 1 settled on, and it is the order an admin
// works in rather than the order the API is grouped in:
//
//   1. **Fields** — what the table *is*. Nothing below it makes sense until the
//      columns do, so it comes first and gets the full width.
//   2. **Table data** — one shallow strip of tiles: how many rows there are, the
//      way through to them, CSV out and CSV in, and a menu for the rare and
//      irreversible. The rows themselves live on their own screen
//      (`TableData`), which is what keeps this page cheap to open: one
//      `countRows` rather than every row in the table.
//   3. **Constraints** — the rules the table's rows are kept to, and the indexes
//      that make reading it faster. Below the fields because every one of them
//      is *about* fields, and above the triggers because a constraint is part of
//      what the table is rather than something that happens to it.
//   4. **Triggers on this table** — what happens when its rows change. Filtered
//      from the trigger list by channel, because a trigger's channel *is* its
//      table (§10.2).
//   5. **Edit table properties** — the `_fd_tables` overlay: labels, roles,
//      ownership. Last because it is the part an admin sets once.
//
// Fields come from `listFields`; the settings come from the tables listing,
// which already carries every overlay field.

import { useEffect, useMemo, useState, type FormEvent, type ReactNode } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Dropdown from "react-bootstrap/Dropdown";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Row from "react-bootstrap/Row";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { dropTableQuestion } from "../history";
import {
  IconArrowLeft,
  IconDots,
  IconDownload,
  IconPencil,
  IconPlus,
  IconUpload,
} from "../icons";
import { AlertBody, PageBody, PageHeader, StatusBadge } from "../layout";
import { anyWrite, writesOf, type TableWrites } from "../tableWrites";
import type {
  ListConstraintsResponse,
  ListFieldsResponse,
  ListFieldTypesResponse,
  ListInboundKeysResponse,
  ListTablesResponse,
  ListTriggersResponse,
} from "../client";
import {
  TEXT_SEARCH_LANGUAGES,
  constraintFormError,
  constraintSummary,
  constraintTypeLabel,
  createConstraintBody,
  newConstraintForm,
  type ConstraintForm,
  type ConstraintItem,
  type ConstraintType,
} from "../constraintForm";
import {
  createFieldBody,
  defaultEditable,
  fieldForm,
  fieldFormError,
  keyValueNote,
  newFieldForm,
  nullableEditable,
  updateFieldBody,
  type FieldForm,
  type FieldItem,
  type FieldKind,
} from "../fieldForm";
import { keyStorage, reconcileKey, type KeyKind } from "../keyField";
import { RoleSelect } from "../roleSelect";
import { useRoles } from "../roles";
import { SettingsFields, buildConfig, initialValues } from "../settings";
import { T, useT } from "../i18n";

/** A one-line description of a field's kind for the fields table. */
function kindLabel(kind: unknown): string {
  const k = kind as FieldKind;
  if (!k || !k.type || k.type === "plain") return "—";
  if (k.type === "file") return `file → ${k.store || "?"}`;
  if (k.type === "key") return `key → ${k.target_table || "?"}`;
  if (k.type === "calc") return `calc: ${k.expression || "?"}`;
  return k.type;
}

/**
 * The table a `Key` field points at, or `null` for every other kind.
 *
 * A key whose target is missing is `null` too rather than a link to nowhere:
 * `kindLabel` renders that case as `key → ?`, and a broken overlay is a thing to
 * read, not a thing to click.
 */
export function keyTarget(kind: unknown): string | null {
  const k = kind as FieldKind;
  if (!k || k.type !== "key") return null;
  return k.target_table || null;
}

/** Where a table's page lives, as the hash router addresses it. */
export function tableHref(table: string): string {
  return `#/tables/${encodeURIComponent(table)}`;
}

/** One `Key` pointing at this table, as `listInboundKeys` reports it. */
type InboundKey = ListInboundKeysResponse[number];

/**
 * The inbound keys gathered by the table they come from.
 *
 * One line per *table*, not per field, because "`time_entries` points here" is
 * the fact an admin is after and a table with two keys onto this one
 * (`author` and `editor`, say) is still one table to open. The server has
 * already left self-joins out (`SchemaProjection::referencing_fields`), so
 * nothing here has to know about them.
 */
export function inboundKeyGroups(keys: InboundKey[]): Array<{ table: string; fields: string[] }> {
  const groups = new Map<string, string[]>();
  for (const k of keys) {
    const fields = groups.get(k.table);
    if (fields) fields.push(k.field);
    else groups.set(k.table, [k.field]);
  }
  return [...groups.entries()]
    .map(([table, fields]) => ({ table, fields }))
    .sort((a, b) => a.table.localeCompare(b.table));
}

/**
 * A field's kind in the list — the same one line `kindLabel` writes, except that
 * a `Key`'s target is a link.
 *
 * The target *is* the interesting half of a key, and following it used to mean
 * reading the name here and then finding it in the Tables list. Everything else
 * a kind can be names something that is not a page (a store, an expression), so
 * this is the only cell with a link in it.
 */
function KindCell({ kind }: { kind: unknown }) {
  const target = keyTarget(kind);
  if (!target) return <>{kindLabel(kind)}</>;
  return (
    <>
      <T text="key" /> &rarr; <a href={tableHref(target)}>{target}</a>
    </>
  );
}

/** The human heading for a field-type category in the picker. */
function categoryLabel(category: string): string {
  if (category === "basic") return "Basic types";
  if (category === "rich") return "Rich types";
  return "Field kinds";
}

/** One table as `listTables` reports it, including its overlay settings. */
type TableSummary = ListTablesResponse[number];

/** One trigger as `listTriggers` reports it. */
type TriggerItem = ListTriggersResponse[number];

/** The events that happen *to a table's rows* — the ones whose channel is a
 * table name, and so the ones this page can show as "on this table" (§10.2). */
const TABLE_EVENTS = ["insert", "update", "delete"];

/**
 * The triggers that fire on `table`'s rows, out of every trigger there is.
 *
 * Both halves matter. The kind must be a **row** event, because a `never` or a
 * `daily` trigger whose *action configuration* happens to name this table is
 * not a trigger on it — it is a trigger that writes to it, which is a different
 * question. And the channel must be this table: for the row events the channel
 * *is* the table (§10.2), so this is the whole binding, and there is nothing
 * else to look at.
 */
export function triggersOnTable(
  triggers: ListTriggersResponse,
  table: string,
): ListTriggersResponse {
  return triggers.filter((t) => TABLE_EVENTS.includes(t.when) && t.channel === table);
}

export function TableDetail({ table }: { table: string }) {
  const [fields, setFields] = useState<ListFieldsResponse | null>(null);
  const [fieldTypes, setFieldTypes] = useState<ListFieldTypesResponse | null>(null);
  const [rowCount, setRowCount] = useState<number | null>(null);
  const [settings, setSettings] = useState<TableSummary | null>(null);
  const [constraints, setConstraints] = useState<ListConstraintsResponse | null>(null);
  // Which other tables hold a `Key` onto this one — the fields card's other
  // direction. Answered by the server rather than assembled here: it is the
  // whole catalog's question, and `listFields` per table would be one request
  // per table to ask it.
  const [inbound, setInbound] = useState<ListInboundKeysResponse | null>(null);
  const [triggers, setTriggers] = useState<TriggerItem[] | null>(null);
  // Every table in the catalog: what a Key field's target is chosen from. This
  // table is included — a key onto its own table (a parent link) is legitimate.
  const [tables, setTables] = useState<ListTablesResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = async () => {
    setError(null);
    try {
      // The settings come from the tables listing rather than a per-table
      // endpoint: the list already carries every overlay field, so a second
      // endpoint would be a second thing to keep in step with it.
      const [f, ft, c, t, tr, cons, inb] = await Promise.all([
        api.listFields(table),
        api.listFieldTypes(),
        api.countRows(table),
        api.listTables(),
        api.listTriggers(),
        api.listConstraints(table),
        api.listInboundKeys(table),
      ]);
      setFields(f);
      setInbound(inb);
      setFieldTypes(ft);
      setRowCount(c.count);
      setTables(t);
      setTriggers(tr);
      setConstraints(cons);
      setSettings(t.find((candidate) => candidate.name === table) ?? null);
    } catch {
      setError("Could not load the table.");
    }
  };

  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [table]);

  return (
    <>
      <PageHeader
        pretitle="Table"
        title={settings?.label || table}
        actions={
          <Button variant="outline-secondary" onClick={() => navigate("/tables")}>
            <IconArrowLeft className="icon-2" />
            <T text="Tables" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}

        {/* A **provided** table's settings come first, above its fields, because
            they are what *decides* the fields: the columns below are the
            module's answer to the form above, and reading them the other way
            round would suggest they could be edited. */}
        {settings?.provider && (
          <ProviderSettings table={table} settings={settings} onChange={load} />
        )}

        <Fields
          table={table}
          fields={fields}
          fieldTypes={fieldTypes}
          tables={tables}
          inbound={inbound}
          provided={Boolean(settings?.provider)}
          metadata={settings?.metadata ?? false}
          module={settings?.module ?? null}
          moduleFields={settings?.module_fields ?? []}
          onChange={load}
        />

        <TableData
          table={table}
          rowCount={rowCount}
          configured={settings?.configured ?? false}
          provided={Boolean(settings?.provider)}
          metadata={settings?.metadata ?? false}
          writes={settings?.provider?.writes ?? null}
          logged={Boolean(settings?.history_enabled || settings?.sync_enabled)}
          onChange={load}
        />

        {/* Constraints are schema, and neither a provider's nor Saltcorn's own
            metadata tables have a schema the admin may change. */}
        {!settings?.provider && !settings?.metadata && (
          <Constraints
            table={table}
            fields={fields}
            constraints={constraints}
            onChange={load}
          />
        )}

        <Triggers table={table} triggers={triggers} onChange={load} />

        <Settings table={table} settings={settings} onChange={load} />
      </PageBody>
    </>
  );
}

/**
 * The table-data strip: how much data there is, and the four things one does
 * with the data rather than with the table.
 *
 * Deliberately **shallow** — a row of tiles, not a panel. Everything on it is a
 * single click that leaves this page or moves a file, so there is nothing to
 * lay out beyond the tiles themselves, and its height is what keeps the fields
 * above it and the properties below it on the same screen.
 */
function TableData({
  table,
  rowCount,
  configured,
  provided,
  metadata,
  writes,
  logged,
  onChange,
}: {
  table: string;
  rowCount: number | null;
  /** Whether row history or offline sync records the table's changes: a drop
   * deletes that record too, and the confirmation says so. */
  logged?: boolean;
  /** Whether a module's table provider serves the rows (§8.3). */
  provided?: boolean;
  /** Whether this is one of Saltcorn's own metadata tables, on the list by the
   * admin's choice: "delete" takes it off the list and drops nothing. */
  metadata?: boolean;
  /** Which writes that provider answers **for the settings this table has**
   * (§8.3) — `null` on a table in a database, where writing is the driver's.
   * The same provider configured read-only answers none of them, so it is read
   * off the table rather than inferred from the provider's name. */
  writes?: TableWrites | null;
  configured: boolean;
  onChange: () => void;
}) {
  const { t } = useT();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  /** One message per row an import refused, each naming its line. */
  const [rejected, setRejected] = useState<string[]>([]);
  // A table in a database writes through its driver and has no `writes` object;
  // a provided one writes only through what its provider answered.
  const allowed = writesOf(writes ? { writes } : null);

  /**
   * Save the table's rows as a CSV file.
   *
   * The document arrives as text in the endpoint's JSON (the endpoint model has
   * no bytes shape, and CSV is text), so the download is made here from a blob
   * and an object URL. The link is clicked and revoked in the same turn — there
   * is nothing to leave in the document, and the SPA is the only thing that
   * ever wanted the URL.
   */
  const download = async () => {
    setBusy(true);
    setError(null);
    setNotice(null);
    setRejected([]);
    try {
      const { filename, csv } = await api.exportTableCsv(table);
      const url = URL.createObjectURL(new Blob([csv], { type: "text/csv;charset=utf-8" }));
      const link = document.createElement("a");
      link.href = url;
      link.download = filename;
      link.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      setError(errorMessage(err, "Could not export the rows."));
    } finally {
      setBusy(false);
    }
  };

  /**
   * Read a chosen CSV file and import it.
   *
   * Both numbers are reported, because both are true of a real file: an import
   * is not a transaction (each row stands on its own), so "42 rows added, 3
   * refused" is the answer, and the three come back with their line numbers so
   * they can be found in the file.
   */
  const upload = async (file: File) => {
    setBusy(true);
    setError(null);
    setNotice(null);
    setRejected([]);
    try {
      const csv = await file.text();
      const { inserted, updated, errors } = await api.importTableCsv(table, { csv });
      setNotice(
        `${inserted} row${inserted === 1 ? "" : "s"} added from ${file.name}` +
          // A file that names primary keys replaces rows as well as adding
          // them, and "0 rows added" for a file that rewrote the whole table
          // would read as nothing having happened.
          (updated > 0 ? `, ${updated} replaced` : "") +
          (errors.length > 0 ? `, ${errors.length} refused.` : "."),
      );
      setRejected(errors);
      onChange();
    } catch (err) {
      // The server's own refusal — a header naming a column the table does not
      // have, a file that is not CSV — says what to fix.
      setError(errorMessage(err, "Could not import the file."));
    } finally {
      setBusy(false);
    }
  };

  /**
   * Drop the table, its columns, its rows and its settings row.
   *
   * Confirmed in the browser because it is the one irreversible thing on this
   * screen — and the server's own refusal (another table's key still points
   * here) is shown as it came, since it names the fields to remove first.
   */
  const dropTable = async () => {
    // A provided table has no rows of Saltcorn's to lose — deleting it forgets
    // the definition and leaves the feed, the remote database or whatever else
    // is behind it exactly where it was — so the warning says what actually
    // happens rather than the one a database table gets.
    const question = metadata
      ? `Remove "${table}" from the tables list? Its settings are forgotten; the table and its rows are Saltcorn's and are not touched.`
      : provided
        ? `Delete the table "${table}"? Saltcorn forgets it. The data it was reading is not touched.`
        : dropTableQuestion(table, Boolean(logged));
    if (!window.confirm(question)) {
      return;
    }
    try {
      await api.dropTable(table);
      navigate("/tables");
    } catch (err) {
      setError(errorMessage(err, "Could not drop the table."));
    }
  };

  /**
   * Delete every row, keeping the table, its fields and its settings.
   *
   * Confirmed with the number that is about to go, because the count is the
   * one thing the admin can check against what they expected before it is too
   * late. A refusal from the server (another table's key still points at one
   * of these rows) is shown as it came, since it names what is in the way.
   */
  const deleteAllRows = async () => {
    if (
      !window.confirm(
        t('Delete all {count} rows of "{name}"? The table and its fields stay. This cannot be undone.', {
          count: String(rowCount ?? 0),
          name: table,
        }),
      )
    ) {
      return;
    }
    setBusy(true);
    setError(null);
    setNotice(null);
    setRejected([]);
    try {
      const { deleted } = await api.deleteAllRows(table);
      setNotice(`${deleted} row${deleted === 1 ? "" : "s"} deleted.`);
      onChange();
    } catch (err) {
      setError(errorMessage(err, "Could not delete the rows."));
    } finally {
      setBusy(false);
    }
  };

  const forget = async () => {
    if (
      !window.confirm(
        t(
          'Forget the settings for "{name}"?\n\nIt returns to admin-only. The table and its rows are not touched.',
          { name: table },
        ),
      )
    ) {
      return;
    }
    setError(null);
    try {
      await api.deleteTableSettings(table);
      onChange();
    } catch (err) {
      setError(errorMessage(err, "Could not forget the settings."));
    }
  };

  return (
    <Card className="mb-4">
      <Card.Header><T text="Table data" /></Card.Header>
      <Card.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {notice && (
          <Alert
            variant={rejected.length > 0 ? "warning" : "success"}
            onClose={() => setNotice(null)}
            dismissible
          >
            <div className="flex-fill">
              <div>{notice}</div>
              {rejected.length > 0 && (
                <ul className="small mb-0 mt-2">
                  {rejected.map((message) => (
                    <li key={message}>{message}</li>
                  ))}
                </ul>
              )}
            </div>
          </Alert>
        )}
        <div className="d-flex flex-wrap align-items-center justify-content-around gap-3 text-center">
          <div>
            <div className="h1 mb-0">{rowCount ?? "—"}</div>
            <div className="text-muted">{rowCount === 1 ? "Row" : "Rows"}</div>
          </div>

          <Tile
            label={anyWrite(allowed) ? "Edit" : "View"}
            icon={<IconPencil />}
            onClick={() => navigate(`/tables/${encodeURIComponent(table)}/data`)}
          />

          <Tile
            label={t("Download CSV")}
            icon={<IconDownload />}
            disabled={busy}
            onClick={() => void download()}
          />

          {/* Importing is inserting, so it is offered exactly when inserting is:
              never on a feed, and on a writable provided table (§8.3) as readily
              as on a table in a database. A disabled tile beside a working one is
              a question that should not have been asked. */}
          {allowed.insert && <UploadTile disabled={busy} onFile={(file) => void upload(file)} />}

          <Dropdown align="end">
            <Dropdown.Toggle
              variant="outline-secondary"
              size="sm"
              className="data-menu-toggle btn-icon"
              id="table-data-menu"
              aria-label={t("More table actions")}
            >
              <IconDots className="icon-2" />
            </Dropdown.Toggle>
            <Dropdown.Menu>
              {/* Not for a provided table: its stored row is not settings added
                  to a table, it *is* the table, so forgetting it would be
                  deleting it — which is the item below, with the confirmation
                  that says so. */}
              {/* Nor for a metadata table, whose row is what puts it on the
                  list: forgetting it is the "remove" below. */}
              {configured && !provided && !metadata && (
                <Dropdown.Item onClick={() => void forget()}><T text="Forget settings" /></Dropdown.Item>
              )}
              {/* Not on a metadata table: its rows are Saltcorn's own — the
                  users table among them — and emptying one in a click would
                  lock the admin out. Row by row, in the data grid, it still can. */}
              {allowed.delete && !metadata && (
                <Dropdown.Item
                  className="text-danger"
                  disabled={busy || !rowCount}
                  onClick={() => void deleteAllRows()}
                >
                  <T text="Delete all rows" />
                </Dropdown.Item>
              )}
              <Dropdown.Item className="text-danger" onClick={() => void dropTable()}>
                {metadata ? "Remove from tables list" : provided ? "Delete table" : "Drop table"}
              </Dropdown.Item>
            </Dropdown.Menu>
          </Dropdown>
        </div>
      </Card.Body>
    </Card>
  );
}

/** One tile of the table-data strip: a large icon over its label (`admin.css`). */
function Tile({
  label,
  icon,
  disabled,
  onClick,
}: {
  label: string;
  icon: ReactNode;
  disabled?: boolean;
  onClick: () => void;
}) {
  return (
    <button type="button" className="data-tile" disabled={disabled} onClick={onClick}>
      {icon}
      <span>{label}</span>
    </button>
  );
}

/**
 * The upload tile: the same shape as the others, over a hidden file input.
 *
 * A file cannot be chosen without one — the browser only opens the picker from
 * a real `<input type="file">` — so the input is present and invisible and the
 * tile is its label. The value is cleared after each pick so that choosing the
 * *same* file again still fires a change.
 */
function UploadTile({
  disabled,
  onFile,
}: {
  disabled?: boolean;
  onFile: (file: File) => void;
}) {
  return (
    <label className="data-tile mb-0">
      <IconUpload />
      <span><T text="Upload CSV" /></span>
      <input
        type="file"
        accept=".csv,text/csv"
        className="d-none"
        disabled={disabled}
        onChange={(e) => {
          const file = e.target.files?.[0];
          e.target.value = "";
          if (file) onFile(file);
        }}
      />
    </label>
  );
}

/**
 * The table's constraints: the rules its rows are kept to, and the indexes that
 * make reading it faster (design §5).
 *
 * Saltcorn 1 puts these behind a "Constraints" link on its table page, on a
 * screen of their own. Here they are a card, because this page is already where
 * a table's *shape* is edited and every constraint is about the fields directly
 * above it — a second screen would be a second place to look for one thing.
 *
 * Four kinds share the card, the list and one modal, and differ only in what the
 * modal asks:
 *
 *   - **Jointly unique** ticks the fields that are unique *together*, which is
 *     the whole point of it — a single field's uniqueness is a property of the
 *     field and is set on the field.
 *   - **Index** picks one field.
 *   - **Full-text search** picks a language and covers every text field there is.
 *   - **Row constraint** takes a formula and a short name.
 *
 * The two that a row can *violate* also take a message, which is what whoever
 * broke the rule is shown instead of Postgres naming a constraint they have
 * never heard of.
 *
 * There is no Edit: what a constraint constrains **is** its identity — a unique
 * key over a different pair of fields is a different rule, and a changed formula
 * is a different trigger — so changing one is deleting it and adding the one you
 * meant, which is also exactly what the database does.
 */
function Constraints({
  table,
  fields,
  constraints,
  onChange,
}: {
  table: string;
  fields: ListFieldsResponse | null;
  constraints: ListConstraintsResponse | null;
  onChange: () => void;
}) {
  const { t } = useT();
  const [form, setForm] = useState<ConstraintForm | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [formError, setFormError] = useState<string | null>(null);

  // A constraint is over stored columns; a calculated field has none.
  const columns = useMemo(
    () => (fields ?? []).filter((f) => (f.kind as FieldKind)?.type !== "calc"),
    [fields],
  );
  const update = (patch: Partial<ConstraintForm>) =>
    setForm((current) => (current ? { ...current, ...patch } : current));

  const open = (type: ConstraintType) => {
    setFormError(null);
    setForm(newConstraintForm(type));
  };

  const save = async (event: FormEvent) => {
    event.preventDefault();
    if (!form) return;
    const invalid = constraintFormError(form);
    if (invalid) {
      setFormError(invalid);
      return;
    }
    setBusy(true);
    setFormError(null);
    try {
      await api.createConstraint(table, createConstraintBody(form));
      setForm(null);
      onChange();
    } catch (err) {
      setFormError(errorMessage(err, "Could not add the constraint."));
    } finally {
      setBusy(false);
    }
  };

  const remove = async (constraint: ConstraintItem) => {
    if (
      !window.confirm(
        t(
          'Delete the {kind} "{name}"?\n\nThe rows already in the table are not changed; only the rule is removed, and rows that would have been refused will be accepted.',
          {
            kind: constraintTypeLabel(constraint.type).toLowerCase(),
            name: constraint.name,
          },
        ),
      )
    ) {
      return;
    }
    setError(null);
    try {
      await api.deleteConstraint(table, constraint.name);
      onChange();
    } catch (err) {
      setError(errorMessage(err, "Could not delete the constraint."));
    }
  };

  const listed = constraints ?? [];
  return (
    <Card className="mb-4">
      <Card.Header><T text="Constraints and indexes" /></Card.Header>
      <Card.Body>
        {error && <Alert variant="danger">{error}</Alert>}        
        {listed.length > 0 && (
          <Table size="sm" hover responsive className="table-vcenter">
            <thead>
              <tr>
                <th><T text="Type" /></th>
                <th><T text="What" /></th>
                <th><T text="Message" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {listed.map((constraint) => (
                <tr key={constraint.name}>
                  <td>
                    {constraintTypeLabel(constraint.type)}
                    <div className="text-muted small font-monospace text-break">
                      {constraint.name}
                    </div>
                  </td>
                  <td className="font-monospace text-break">{constraintSummary(constraint)}</td>
                  <td className="text-break">
                    {constraint.error_message ?? <span className="text-muted">—</span>}
                  </td>
                  <td className="text-end">
                    <div className="btn-list justify-content-end flex-nowrap">
                      {!constraint.managed && (
                        // Not made here, so it is somebody else's migration —
                        // deletable, but said so before it is deleted.
                        <StatusBadge tone="secondary" title={t("Not created by Saltcorn")}>
                          <T text="External" />
                        </StatusBadge>
                      )}
                      <Button
                        size="sm"
                        variant="outline-danger"
                        onClick={() => void remove(constraint)}
                      >
                        <T text="Delete" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
        <div className="btn-list">
          <Button size="sm" onClick={() => open("unique")}>
            <IconPlus className="icon-2" />
            <T text="Jointly unique" />
          </Button>
          <Button size="sm" variant="outline-secondary" onClick={() => open("index")}>
            <IconPlus className="icon-2" />
            <T text="Index" />
          </Button>
          <Button
            size="sm"
            variant="outline-secondary"
            onClick={() => open("full_text_search")}
          >
            <IconPlus className="icon-2" />
            <T text="Full-text search" />
          </Button>
          <Button size="sm" variant="outline-secondary" onClick={() => open("formula")}>
            <IconPlus className="icon-2" />
            <T text="Row constraint" />
          </Button>
        </div>

        <Modal show={form !== null} onHide={() => setForm(null)} scrollable>
          <Form onSubmit={save}>
            <Modal.Header closeButton>
              <Modal.Title className="h4">
                {form ? `Add ${constraintTypeLabel(form.type).toLowerCase()} to ${table}` : ""}
              </Modal.Title>
            </Modal.Header>
            <Modal.Body>
              {formError && <Alert variant="danger">{formError}</Alert>}
              {form?.type === "unique" && (
                <Form.Group className="mb-2">
                  <Form.Label><T text="Fields that are unique together" /></Form.Label>
                  {columns.map((field) => (
                    <Form.Check
                      key={field.name}
                      type="checkbox"
                      id={`unique-${field.name}`}
                      label={field.label || field.name}
                      checked={form.fields.includes(field.name)}
                      onChange={(e) =>
                        update({
                          fields: e.target.checked
                            ? [...form.fields, field.name]
                            : form.fields.filter((f) => f !== field.name),
                        })
                      }
                    />
                  ))}
                  <Form.Text muted>
                    <T text="No two rows may share the same combination of these fields." />
                  </Form.Text>
                </Form.Group>
              )}
              {form?.type === "index" && (
                <Form.Group className="mb-2" controlId="constraintIndexField">
                  <Form.Label><T text="Field" /></Form.Label>
                  <Form.Select
                    value={form.fields[0] ?? ""}
                    onChange={(e) => update({ fields: e.target.value ? [e.target.value] : [] })}
                  >
                    <option value=""><T text="Choose a field…" /></option>
                    {columns.map((field) => (
                      <option key={field.name} value={field.name}>
                        {field.label || field.name}
                      </option>
                    ))}
                  </Form.Select>
                  <Form.Text muted>
                    <T text="An index makes searching and joining on this field faster, and writes a little slower." />
                  </Form.Text>
                </Form.Group>
              )}
              {form?.type === "full_text_search" && (
                <Form.Group className="mb-2" controlId="constraintLanguage">
                  <Form.Label><T text="Language" /></Form.Label>
                  <Form.Select
                    value={form.language}
                    onChange={(e) => update({ language: e.target.value })}
                  >
                    {TEXT_SEARCH_LANGUAGES.map((language) => (
                      <option key={language} value={language}>
                        {language}
                      </option>
                    ))}
                  </Form.Select>
                  <Form.Text muted>
                    <T text="Indexes every text field of the table together. The language decides how words are reduced to their stems, so it has to match the one a search uses." />
                  </Form.Text>
                </Form.Group>
              )}
              {form?.type === "formula" && (
                <>
                  <Form.Group className="mb-2" controlId="constraintName">
                    <Form.Label><T text="Name" /></Form.Label>
                    <Form.Control
                      value={form.name}
                      autoFocus
                      placeholder="positive_salary"
                      onChange={(e) => update({ name: e.target.value })}
                    />
                    <Form.Text muted>
                      <T text="A short name for this rule. It appears in the error when no message is given." />
                    </Form.Text>
                  </Form.Group>
                  <Form.Group className="mb-2" controlId="constraintFormula">
                    <Form.Label><T text="Formula" /></Form.Label>
                    <Form.Control
                      as="textarea"
                      rows={2}
                      className="font-monospace"
                      value={form.formula}
                      placeholder={t("salary > 0")}
                      onChange={(e) => update({ formula: e.target.value })}
                    />
                    <Form.Text muted>
                      <T
                        text="Must be true of every row. In scope: {fields}."
                        args={{
                          fields:
                            columns.map((f) => f.name).join(", ") ||
                            t("no fields yet"),
                        }}
                      />{" "}
                      <T
                        text="Join fields ({join}) and aggregations ({aggregation}) may be used; {user} may not, because the database checks this and has no session."
                        values={{
                          join: <code>authorⱵname</code>,
                          aggregation: <code>reviewsↃbook.length</code>,
                          user: <code>user</code>,
                        }}
                      />
                    </Form.Text>
                  </Form.Group>
                </>
              )}
              {form && form.type !== "index" && form.type !== "full_text_search" && (
                <Form.Group className="mb-2" controlId="constraintMessage">
                  <Form.Label><T text="Error message" /></Form.Label>
                  <Form.Control
                    value={form.errorMessage}
                    onChange={(e) => update({ errorMessage: e.target.value })}
                  />
                  <Form.Text muted>
                    <T text="Shown to whoever breaks the rule. Left blank, they see the database's own message, which names the constraint." />
                  </Form.Text>
                </Form.Group>
              )}
            </Modal.Body>
            <Modal.Footer>
              <Button variant="outline-secondary" onClick={() => setForm(null)}>
                <T text="Cancel" />
              </Button>
              <Button type="submit" disabled={busy}>
                {busy ? "Adding…" : "Add constraint"}
              </Button>
            </Modal.Footer>
          </Form>
        </Modal>
      </Card.Body>
    </Card>
  );
}

/**
 * The triggers that fire on this table's rows (§10.2).
 *
 * Filtered from the whole trigger list rather than fetched by table, because a
 * trigger's **channel is its table** for the three row events — there is no
 * separate binding to query. Everything about a trigger is edited on the
 * trigger form; this card exists so that "what happens when a row of this table
 * changes?" is answerable from the table, which is where it is asked.
 */
function Triggers({
  table,
  triggers,
  onChange,
}: {
  table: string;
  triggers: TriggerItem[] | null;
  onChange: () => void;
}) {
  const { t } = useT();
  const [error, setError] = useState<string | null>(null);
  const mine = useMemo(() => triggersOnTable(triggers ?? [], table), [triggers, table]);

  const remove = async (trigger: TriggerItem) => {
    if (
      !window.confirm(
        t(
          'Delete the trigger "{name}"?\n\nIts configuration is deleted with it. Switch it off instead if you only want it to stop firing.',
          { name: trigger.name },
        ),
      )
    ) {
      return;
    }
    setError(null);
    try {
      await api.deleteTrigger(trigger.id);
      onChange();
    } catch (err) {
      setError(errorMessage(err, "Could not delete the trigger."));
    }
  };

  return (
    <Card className="mb-4">
      <Card.Header><T text="Triggers on this table" /></Card.Header>
      <Card.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        <p className="text-muted"><T text="Triggers run actions in response to events on this table." /></p>
        {mine.length > 0 && (
          <Table size="sm" hover responsive className="table-vcenter">
            <thead>
              <tr>
                <th><T text="Name" /></th>
                <th><T text="Event" /></th>
                <th><T text="Runs" /></th>
                <th><T text="Status" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {mine.map((trigger) => (
                <tr key={trigger.id}>
                  <td>
                    {trigger.name}
                    {trigger.description && (
                      <div className="text-muted small">{trigger.description}</div>
                    )}
                  </td>
                  <td>
                    {trigger.when}
                    {trigger.only_if && (
                      <div className="text-muted small font-monospace text-break">
                        {t("if {condition}", { condition: trigger.only_if })}
                      </div>
                    )}
                  </td>
                  {/* A workflow body runs a program, not an action (§10.3). */}
                  <td className="text-break">{trigger.action ?? "workflow"}</td>
                  <td>
                    {trigger.error ? (
                      <StatusBadge tone="red" title={trigger.error}>
                        <T text="Not usable" />
                      </StatusBadge>
                    ) : trigger.enabled ? (
                      <StatusBadge tone="green"><T text="Enabled" /></StatusBadge>
                    ) : (
                      <StatusBadge tone="secondary"><T text="Off" /></StatusBadge>
                    )}
                  </td>
                  <td className="text-end">
                    <div className="btn-list justify-content-end flex-nowrap">
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        href={`#/triggers/${encodeURIComponent(trigger.id)}/edit`}
                      >
                        <T text="Edit" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-danger"
                        onClick={() => void remove(trigger)}
                      >
                        <T text="Delete" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}
        <Button
          size="sm"
          onClick={() => navigate(`/triggers/new/${encodeURIComponent(table)}`)}
        >
          <IconPlus className="icon-2" />
          <T text="Create trigger" />
        </Button>
      </Card.Body>
    </Card>
  );
}

/**
 * A **provided** table's provider: which one serves it, what it was configured
 * with, and what is wrong (design §8.3).
 *
 * Above the fields, because it is what decides them: the columns below this card
 * are the module's answer to the form on it, and a save that changes the answer
 * changes them. Saving reloads the catalog, which is what asks the module again.
 *
 * The form is the module's own — declared by the provider's v1
 * `configuration_workflow` and rendered by `SettingsFields`, the same component
 * a file store's backend and an agent's provider use — so **no code here knows
 * what an RSS feed is**.
 */
function ProviderSettings({
  table,
  settings,
  onChange,
}: {
  table: string;
  settings: TableSummary;
  onChange: () => void;
}) {
  const provider = settings.provider;
  const spec = provider?.config_spec ?? [];
  const stored = (provider?.configuration ?? {}) as Record<string, string>;
  const [values, setValues] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  // Re-seeded whenever the stored configuration changes — after a save, and
  // after a module reload changed what the provider asks for.
  useEffect(() => {
    setValues(initialValues(spec, stored));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [JSON.stringify(stored), JSON.stringify(spec.map((f) => f.name))]);

  if (!provider) return null;

  const save = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    setSaved(false);
    try {
      await api.updateProvidedTable(table, { configuration: buildConfig(spec, values) });
      setSaved(true);
      onChange();
    } catch (err) {
      setError(errorMessage(err, "Could not save the provider settings."));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card className="mb-4">
      <Card.Header>
        <T text="Table provider" />
        <StatusBadge tone="blue" className="ms-2">
          {provider.provider}
        </StatusBadge>
        <span className="text-muted small ms-2">{provider.module}</span>
      </Card.Header>
      <Card.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {saved && (
          <Alert variant="success" dismissible onClose={() => setSaved(false)}>
            <T text="Saved. The columns below are what the provider reports for these settings." />
          </Alert>
        )}
        {/* Why this table has no columns, when it has none. The module may be
            uninstalled, its provider renamed, its `fields(cfg)` throwing — all
            of which leave the table listed and fixable, which is the point. */}
        {provider.issues.length > 0 && (
          <Alert variant="warning">
            <AlertBody>
              <Alert.Heading className="h6"><T text="This provider is not answering" /></Alert.Heading>
              <ul className="mb-0">
                {provider.issues.map((issue) => (
                  <li key={issue}>{issue}</li>
                ))}
              </ul>
            </AlertBody>
          </Alert>
        )}
        <p className="text-muted">
          <T text="The rows of this table come from" /> <code>{provider.module}</code><T text=", not from a database. Saltcorn reads them; it does not create, change or delete them." />
        </p>
        <Form onSubmit={save}>
          {spec.length === 0 ? (
            <p className="text-muted mb-3"><T text="This provider asks for no settings." /></p>
          ) : (
            <SettingsFields
              spec={spec}
              values={values}
              idPrefix="table-provider"
              onChange={(name, value) => setValues((v) => ({ ...v, [name]: value }))}
            />
          )}
          <Button type="submit" disabled={busy || spec.length === 0}>
            <T text="Save provider settings" />
          </Button>
        </Form>
      </Card.Body>
    </Card>
  );
}

/**
 * The table's settings: the `_fd_tables` overlay (design §9).
 *
 * Everything here is *added* to what the database already says about the table —
 * nothing on this card restates a column, a type or a key, because those are the
 * database's to state and the overlay's to leave alone.
 *
 * The two roles are the point of the card. Until they can be set, every table is
 * admin-only and an application cannot serve anyone but its admin.
 */
function Settings({
  table,
  settings,
  onChange,
}: {
  table: string;
  settings: TableSummary | null;
  onChange: () => void;
}) {
  const { t } = useT();
  const roles = useRoles();
  const [label, setLabel] = useState("");
  const [description, setDescription] = useState("");
  const [read, setRead] = useState(1);
  const [write, setWrite] = useState(1);
  const [formula, setFormula] = useState("");
  const [rls, setRls] = useState(false);
  const [sync, setSync] = useState(false);
  const [history, setHistory] = useState(false);
  const [historyRole, setHistoryRole] = useState(1);
  // Days a change is kept; 0 is forever.
  const [keepDays, setKeepDays] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  // Re-seed the form whenever the loaded table changes — including after a save,
  // so what is shown is what the server stored rather than what was typed.
  useEffect(() => {
    setLabel(settings?.label ?? "");
    setDescription(settings?.description ?? "");
    setRead(settings?.min_role_read ?? 1);
    setWrite(settings?.min_role_write ?? 1);
    setFormula(settings?.ownership_formula ?? "");
    setRls(settings?.rls_enabled ?? false);
    setSync(settings?.sync_enabled ?? false);
    setHistory(settings?.history_enabled ?? false);
    setHistoryRole(settings?.history_min_role ?? 1);
    setKeepDays(settings?.history_keep_days ?? 0);
    setSaved(false);
  }, [settings]);

  const save = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await api.updateTable(table, {
        label: label.trim(),
        description: description.trim(),
        min_role_read: read,
        min_role_write: write,
        ownership_formula: formula.trim(),
        rls_enabled: rls,
        sync_enabled: sync,
        history_enabled: history,
        history_min_role: history ? historyRole : 1,
        history_keep_days: history ? keepDays : 0,
      });
      setSaved(true);
      onChange();
    } catch (err) {
      // The server's validation message names what to fix — an unknown
      // identifier, a broken Ⱶ-path, a formula RLS cannot enforce.
      setError(err instanceof Error ? err.message : "Could not save the settings.");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Card className="mb-4">
      <Card.Header><T text="Edit table properties" /></Card.Header>
      <Card.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {saved && !error && (
          <Alert variant="success" className="py-2">
            <T text="Saved. The new rules apply immediately — no restart." />
          </Alert>
        )}
        <Form onSubmit={save}>
          <Row>
            <Col md={6}>
              <Form.Group className="mb-3" controlId="tableLabel">
                <Form.Label><T text="Label" /></Form.Label>
                <Form.Control
                  value={label}
                  placeholder={table}
                  onChange={(e) => setLabel(e.target.value)}
                />
                <Form.Text muted><T text="Shown instead of the table name. Blank uses the name." /></Form.Text>
              </Form.Group>
            </Col>
            <Col md={6}>
              <Form.Group className="mb-3" controlId="tableDescription">
                <Form.Label><T text="Description" /></Form.Label>
                <Form.Control
                  value={description}
                  onChange={(e) => setDescription(e.target.value)}
                />
              </Form.Group>
            </Col>
          </Row>
          <Row>
            <Col md={6}>
              <RoleSelect
                id="tableReadRole"
                label={t("Minimum role for full read access")}
                value={read}
                roles={roles}
                onChange={setRead}
              />
            </Col>
            <Col md={6}>
              <RoleSelect
                id="tableWriteRole"
                label={t("Minimum role for full write access")}
                value={write}
                roles={roles}
                onChange={setWrite}
              />
            </Col>
          </Row>
          <Form.Group className="mb-3" controlId="tableOwnershipFormula">
            <Form.Label><T text="Ownership formula" /></Form.Label>
            <Form.Control
              as="textarea"
              rows={2}
              className="font-monospace"
              value={formula}
              placeholder={t("Example: owner === user.id")}
              onChange={(e) => setFormula(e.target.value)}
            />
            <Form.Text muted>
              <T
                text="A JavaScript expression over the row’s fields, {user}, the operation flags ({read}, {write}, …) and Ⱶ-joinfields. Rows it grants are reachable below the roles above. Blank means roles only."
                values={{
                  user: <code>user</code>,
                  read: <code>_read</code>,
                  write: <code>_write</code>,
                }}
              />
            </Form.Text>
            {settings?.ownership_error && (
              <Alert variant="warning" className="py-2 mt-2 mb-0">
                {t("Stored formula is not in effect (it grants nothing): {problem}", {
                  problem: settings.ownership_error,
                })}
              </Alert>
            )}
          </Form.Group>
          {settings?.rls_available && (
            <Form.Group className="mb-3" controlId="tableRlsEnabled">
              <Form.Check
                type="switch"
                label={t("Enforce with database row-level security")}
                checked={rls}
                onChange={(e) => setRls(e.target.checked)}
              />
              <Form.Text muted>
                <T text="The formula becomes Postgres RLS policies enforced by the database itself. Needs a formula the database can evaluate." />
              </Form.Text>
            </Form.Group>
          )}
          {settings?.sync_available && (
            <Form.Group className="mb-3" controlId="tableSyncEnabled">
              <Form.Check
                type="switch"
                label={t("Offline sync")}
                checked={sync}
                onChange={(e) => setSync(e.target.checked)}
              />
              <Form.Text muted>
                <T text="Records every change to this table, so apps can keep an offline copy and sync it. Rows created offline need a uuid key." />
              </Form.Text>
            </Form.Group>
          )}
          {settings?.history_available && (
            <Form.Group className="mb-3" controlId="tableHistoryEnabled">
              <Form.Check
                type="switch"
                label={t("Keep row history")}
                checked={history}
                onChange={(e) => setHistory(e.target.checked)}
              />
              <Form.Text muted>
                <T text="Keeps every version of each row, so you can see who changed what and restore an earlier version." />
              </Form.Text>
              {history && (
                <div className="mt-2">
                  <RoleSelect
                    id="tableHistoryRole"
                    label={t("Minimum role to read the history")}
                    value={historyRole}
                    roles={roles}
                    onChange={setHistoryRole}
                  />
                  <Form.Text muted>
                    <T text="Admin only by default. A wider role sees only the history of rows its read access lets it see, and secrets stay hidden. The admin screens stay admin-only." />
                  </Form.Text>
                  <Form.Group className="mt-2" controlId="tableHistoryKeepDays">
                    <Form.Label><T text="Keep history for (days)" /></Form.Label>
                    <Form.Control
                      type="number"
                      min={0}
                      value={keepDays}
                      onChange={(e) => setKeepDays(Math.max(0, Math.floor(Number(e.target.value) || 0)))}
                    />
                    <Form.Text muted>
                      <T text="0 keeps it forever. Older changes are removed, and with them the files of deleted rows." />
                    </Form.Text>
                  </Form.Group>
                </div>
              )}
            </Form.Group>
          )}
          <Button type="submit" size="sm" disabled={busy}>
            <T text="Save settings" />
          </Button>
          {settings?.configured && (
            <Form.Text muted className="d-block mt-2">
              &ldquo;<T text="Forget settings”, on the table-data menu above, returns the table to admin-only. It never touches the table or its rows." />
            </Form.Text>
          )}
        </Form>
      </Card.Body>
    </Card>
  );
}

/** What the field modal is open on: a new field, or one that exists. */
type Editing = { mode: "add" } | { mode: "edit"; field: string };

/**
 * The fields (columns) panel: list, add and edit (design §3.4).
 *
 * The card is the **list**; adding and editing are the same modal behind the
 * "Add field" button and each row's Edit button. The form is the taller of the
 * two by some way — a Key's three dependent selects, a rich type's whole
 * attribute spec — and side by side it either squeezed the list into half a page
 * or left a column of white space under it, depending on which type was chosen.
 * In a modal it can be as tall as it needs to be, and the card goes back to
 * being the reference an admin reads.
 *
 * One modal for both, because a field is one thing to describe however it got
 * here, and the alternative is two forms that have to agree about where a File's
 * parameters live. What differs is only what may be changed: an edit writes the
 * `_fd_fields` overlay and nothing else (§3.3), so the name and the NOT NULL are
 * shown disabled rather than offered and quietly dropped.
 *
 * The type input is a pick-list assembled from `listFieldTypes` — basic types,
 * rich types and the Key/File kinds in one list — and choosing a type renders
 * **its** declared attribute form beneath, driven entirely by the type's
 * `config_spec` with no per-type code here. That is the same "settings as data"
 * move a file-store backend's form is built on: a rich type or a kind added to
 * the server's registry gets a working form with no change to this file.
 */
function Fields({
  table,
  fields,
  fieldTypes,
  tables,
  inbound,
  provided,
  metadata,
  module = null,
  moduleFields = [],
  onChange,
}: {
  table: string;
  fields: ListFieldsResponse | null;
  fieldTypes: ListFieldTypesResponse | null;
  tables: ListTablesResponse | null;
  /** The `Key` fields on *other* tables that point here (self-joins excluded —
   * a table's key onto itself is already a row in the list above). */
  inbound: ListInboundKeysResponse | null;
  /** Whether a module's table provider decides the columns (§8.3). They are
   * shown, because they are what the table *is*; they are not editable, because
   * there is no column in any database to edit. */
  provided?: boolean;
  /** Whether this is one of Saltcorn's own metadata tables. Its columns are
   * shown and are not editable: the schema is the server's. */
  metadata?: boolean;
  /** The installed module that declared this table (TODO-imap M4.3), and the
   * fields it declared: those cannot be deleted while it is installed. Fields
   * an admin added are theirs. */
  module?: string | null;
  moduleFields?: string[];
  onChange: () => void;
}) {
  const { t } = useT();
  // Either way the columns are someone else's to decide.
  const readOnly = provided || metadata;
  /** What the modal is open on, or `null` when it is closed. */
  const [editing, setEditing] = useState<Editing | null>(null);
  const [form, setForm] = useState<FieldForm>(newFieldForm(""));
  const [targetFields, setTargetFields] = useState<ListFieldsResponse | null>(null);
  const [busy, setBusy] = useState(false);
  // Two error slots, because the two things this card does now happen in two
  // places: a refused drop belongs on the card, beside the row it was about,
  // and everything the field form can be told belongs in the modal the admin is
  // looking at.
  const [error, setError] = useState<string | null>(null);
  const [formError, setFormError] = useState<string | null>(null);

  const update = (patch: Partial<FieldForm>) => setForm((f) => ({ ...f, ...patch }));

  /** Whether the modal is editing a field that exists — which is what decides
   * what it may change, not just what it says at the top. */
  const isEdit = editing?.mode === "edit";
  const selected = fieldTypes?.find((t) => t.name === form.typeName) ?? null;
  const basicTypes = useMemo(
    () => (fieldTypes ?? []).filter((t) => t.category === "basic"),
    [fieldTypes],
  );
  // A calc field has no column, so a kind (Key/File) makes no sense for it —
  // the picker offers only value types (basic + rich) while it is checked.
  const typeCategories = form.calculated ? ["basic", "rich"] : ["basic", "rich", "kind"];

  // Default the picker to the first type once the list loads.
  useEffect(() => {
    if (!form.typeName && fieldTypes && fieldTypes.length > 0) {
      update({ typeName: fieldTypes[0].name });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fieldTypes, form.typeName]);

  // Turning on "calculated" while a kind is selected would leave an invalid
  // pairing; fall back to the first value type.
  useEffect(() => {
    if (form.calculated && selected?.category === "kind") {
      update({ typeName: basicTypes[0]?.name ?? "" });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [form.calculated, selected, basicTypes]);

  // The chosen target table's own fields, so the target and summary selects
  // offer what that table actually has. Loaded here rather than from the tables
  // listing because `listTables` reports settings, not columns — and a table's
  // fields change under this screen as often as they are edited on it.
  useEffect(() => {
    const target = form.key.target_table;
    if (!target) {
      setTargetFields(null);
      return;
    }
    let cancelled = false;
    api
      .listFields(target)
      .then((list) => {
        if (cancelled) return;
        setTargetFields(list);
        // Re-check the selects against what the table has: a target field left
        // over from the previously chosen table would name a column of the
        // wrong one.
        setForm((f) =>
          f.key.target_table === target ? { ...f, key: reconcileKey(f.key, list) } : f,
        );
      })
      .catch(() => {
        if (!cancelled) setFormError(`Could not read the fields of “${target}”.`);
      });
    return () => {
      cancelled = true;
    };
  }, [form.key.target_table]);

  /** Open the modal on an empty form — never on what the last one left behind. */
  const openAdd = () => {
    setForm(newFieldForm(fieldTypes?.[0]?.name ?? ""));
    setFormError(null);
    setEditing({ mode: "add" });
  };

  /** Open the modal on a field that exists, seeded from what the server says it
   * is — so saving without touching anything is a no-op rather than a reset. */
  const openEdit = (field: FieldItem) => {
    setForm(fieldForm(field));
    setFormError(null);
    setEditing({ mode: "edit", field: field.name });
  };

  const save = async (e: FormEvent) => {
    e.preventDefault();
    if (!editing || !form.name.trim() || !selected) return;
    const invalid = fieldFormError(form, selected);
    if (invalid) {
      setFormError(invalid);
      return;
    }
    setBusy(true);
    setFormError(null);
    try {
      if (editing.mode === "edit") {
        await api.updateField(table, editing.field, updateFieldBody(form, selected));
      } else {
        await api.createField(table, createFieldBody(form, selected));
      }
      // The field is in the list behind the modal now, so the modal's work is
      // done. A refusal leaves it open, on the values that were refused.
      setEditing(null);
      onChange();
    } catch (err) {
      setFormError(
        errorMessage(
          err,
          editing.mode === "edit" ? "Could not save the field." : "Could not add the field.",
        ),
      );
    } finally {
      setBusy(false);
    }
  };

  /**
   * Drop a field: the column, its data and its settings row.
   *
   * The server's refusal is shown as it came rather than replaced with a generic
   * message — "it is referenced by `matters.client`" is the only version of
   * "no" that says what to do next.
   */
  const drop = async (field: string) => {
    if (
      !window.confirm(
        t('Drop the field "{name}" and the data in it? This cannot be undone.', {
          name: field,
        }),
      )
    ) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await api.deleteField(table, field);
      onChange();
    } catch (err) {
      setError(errorMessage(err, "Could not drop the field."));
    } finally {
      setBusy(false);
    }
  };

  // Nothing invents a primary key (GOALS), so a table can genuinely be without
  // one — and a table without one cannot be edited row by row, cannot be
  // referenced by another table's key and cannot be upserted into from a CSV.
  // That is worth saying loudly and exactly once: here, over the fields, which
  // is where it is fixed. `fields === null` is "not loaded yet", not "no key".
  const hasPrimaryKey = fields === null || fields.some((f) => f.primary_key);

  // One line per table that points here, whatever number of keys it does it
  // with.
  const groups = useMemo(() => inboundKeyGroups(inbound ?? []), [inbound]);

  return (
    <Card className="mb-4">
      <Card.Header><T text="Fields" /></Card.Header>
      <Card.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {!hasPrimaryKey && (
          <Alert variant="danger">
            <AlertBody>
              <Alert.Heading className="h6"><T text="This table has no primary key" /></Alert.Heading>
              <p className="mb-0">
                <T text="Create a field and tick" /> <strong><T text="Primary key" /></strong> <T text="to enable table edits and keys fields ferencing this table." />
              </p>
            </AlertBody>
          </Alert>
        )}
        <Table size="sm" hover responsive className="table-vcenter mb-3">
          <thead>
            <tr>
              <th><T text="Name" /></th>
              <th><T text="Type" /></th>
              <th><T text="Kind" /></th>
              <th><T text="Nullable" /></th>
              <th><T text="Key" /></th>
              <th className="text-end"><T text="Actions" /></th>
            </tr>
          </thead>
          <tbody>
            {fields?.length === 0 && (
              <tr>
                <td colSpan={6} className="text-muted">
                  <T text="No fields yet." />
                </td>
              </tr>
            )}
            {fields?.map((f) => (
              <tr key={f.name}>
                <td>{f.name}</td>
                <td>
                  <code>{f.type}</code>
                </td>
                <td>
                  <KindCell kind={f.kind} />
                </td>
                <td>{f.nullable ? "yes" : "no"}</td>
                <td>
                  {f.primary_key && (
                    <StatusBadge tone="blue" title={t("Part of the primary key")}>
                      <T text="key" />
                    </StatusBadge>
                  )}
                </td>
                <td className="text-end">
                  {!readOnly && (
                    <div className="btn-list justify-content-end flex-nowrap">
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        disabled={busy}
                        onClick={() => openEdit(f)}
                      >
                        <T text="Edit" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-danger"
                        disabled={busy || moduleFields.includes(f.name)}
                        title={
                          moduleFields.includes(f.name)
                            ? t("Declared by the module {module}; it cannot be deleted while the module is installed.", { module: module ?? "" })
                            : undefined
                        }
                        onClick={() => void drop(f.name)}
                      >
                        <T text="Delete" />
                      </Button>
                    </div>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </Table>
        {metadata ? (
          <div className="text-muted small">
            <T text="This is one of Saltcorn’s own metadata tables. Its rows and settings can be edited; its columns are Saltcorn’s and cannot be." />
          </div>
        ) : provided ? (
          <div className="text-muted small">
            <T text="These columns are the table provider’s. Change what it presents in its settings above, or in the module itself." />
          </div>
        ) : (
          <Button size="sm" onClick={openAdd}>
            <IconPlus className="icon-2" />
            <T text="Add field" />
          </Button>
        )}

        {/* The other direction, under the fields rather than in a card of its
            own: it is the same question the list above answers — what is this
            table joined to? — read from the far end. Absent entirely when
            nothing points here, because "no inbound keys" is the ordinary case
            and an empty heading is noise. */}
        {groups.length > 0 && (
          <div className="mt-4">
            <div className="fw-bold mb-1"><T text="Referenced by" /></div>
            <ul className="list-unstyled mb-0 small">
              {groups.map((g) => (
                <li key={g.table}>
                  <a href={tableHref(g.table)}>{g.table}</a>
                  <span className="text-muted"> &middot; {g.fields.join(", ")}</span>
                </li>
              ))}
            </ul>
          </div>
        )}

        <Modal show={editing !== null} onHide={() => setEditing(null)} size="lg" scrollable>
          {/* The form wraps the whole modal so that the footer's button is the
              form's submit and Return in a text box does what the button does. */}
          <Form onSubmit={save}>
            <Modal.Header closeButton>
              <Modal.Title className="h4">
                {isEdit ? `Edit ${table}.${form.name}` : `Add field to ${table}`}
              </Modal.Title>
            </Modal.Header>
            <Modal.Body>
              {formError && <Alert variant="danger">{formError}</Alert>}
              <Row>
                <Col md={6}>
                  <Form.Group className="mb-2" controlId="fieldName">
                    <Form.Label><T text="Name" /></Form.Label>
                    <Form.Control
                      value={form.name}
                      autoFocus={!isEdit}
                      // A rename is a migration, which `updateField` does not do
                      // (§3.3) — shown rather than hidden, because the name is
                      // the first thing that says which field this is.
                      disabled={isEdit}
                      onChange={(e) => update({ name: e.target.value })}
                    />
                    {isEdit && <Form.Text muted><T text="A field cannot be renamed." /></Form.Text>}
                  </Form.Group>
                </Col>
                <Col md={6}>
                  <Form.Group className="mb-2" controlId="fieldLabel">
                    <Form.Label><T text="Label" /></Form.Label>
                    <Form.Control
                      value={form.label}
                      placeholder={form.name}
                      onChange={(e) => update({ label: e.target.value })}
                    />
                    <Form.Text muted><T text="Shown instead of the name. Blank uses the name." /></Form.Text>
                  </Form.Group>
                </Col>
              </Row>
              <Form.Group className="mb-2" controlId="fieldDescription">
                <Form.Label><T text="Description" /></Form.Label>
                <Form.Control
                  value={form.description}
                  onChange={(e) => update({ description: e.target.value })}
                />
              </Form.Group>
              <Form.Group className="mb-2" controlId="fieldType">
                <Form.Label>{form.calculated ? "Value type" : "Type"}</Form.Label>
                <Form.Select
                  value={form.typeName}
                  // A different type has different attributes, so what was
                  // entered for the last one is cleared rather than sent under
                  // names the new type does not have.
                  onChange={(e) => update({ typeName: e.target.value, attrs: {} })}
                >
                  {typeCategories.map((category) => {
                    const items = (fieldTypes ?? []).filter((t) => t.category === category);
                    if (items.length === 0) return null;
                    return (
                      <optgroup key={category} label={categoryLabel(category)}>
                        {items.map((t) => (
                          <option key={t.name} value={t.name}>
                            {t.label}
                          </option>
                        ))}
                      </optgroup>
                    );
                  })}
                </Form.Select>
                {form.calculated ? (
                  <Form.Text muted>
                    <T text="How the computed value is shown. Its real type comes from the expression." />
                  </Form.Text>
                ) : (
                  isEdit && (
                    <Form.Text muted>
                      <T text="The column's storage is unchanged — this is how its value is read and shown." />
                    </Form.Text>
                  )
                )}
              </Form.Group>

              <Form.Check
                className="mb-2"
                id="fieldCalculated"
                type="checkbox"
                label={t("Calculated (computed on read, no stored column)")}
                checked={form.calculated}
                // Whether a field has a column is settled when it is made:
                // turning this on would leave a column nothing reads, and off
                // would leave a field with no column at all.
                disabled={isEdit}
                onChange={(e) => update({ calculated: e.target.checked })}
              />

              {form.calculated ? (
                <Form.Group className="mb-1" controlId="fieldExpression">
                  <Form.Label><T text="Formula" /></Form.Label>
                  <Form.Control
                    as="textarea"
                    rows={2}
                    className="font-monospace"
                    value={form.expression}
                    placeholder={t("pages * 2")}
                    onChange={(e) => update({ expression: e.target.value })}
                  />
                  <Form.Text muted>
                    <T text="A JavaScript expression over the row's fields, Ⱶ-joinfields, Ↄ-aggregations and other calculated fields — never" /> <code>user</code> <T text="or the operation flags." />
                  </Form.Text>
                </Form.Group>
              ) : (
                <>
                  {selected?.name === "key" ? (
                    // A Key's parameters are the one set the spec-driven form cannot
                    // render: each depends on the one above it.
                    <KeyFields
                      tables={tables}
                      targetFields={targetFields}
                      value={form.key}
                      onChange={(key) => update({ key })}
                    />
                  ) : (
                    /* The chosen type's own attributes / a kind's parameters, rendered
                       from its spec — no per-type code lives here. */
                    <SettingsFields
                      spec={selected?.config_spec ?? []}
                      values={form.attrs}
                      onChange={(key, v) => setForm((f) => ({ ...f, attrs: { ...f.attrs, [key]: v } }))}
                      idPrefix="field-attr"
                    />
                  )}

                  <Form.Check
                    className="mb-1"
                    id="fieldNullable"
                    type="checkbox"
                    label={t("Nullable")}
                    checked={form.nullable && !form.primaryKey}
                    // A key column is NOT NULL whatever this says, so it shows
                    // that way and cannot be changed while it is in the key.
                    disabled={!nullableEditable(form)}
                    onChange={(e) => update({ nullable: e.target.checked })}
                  />
                  {isEdit && form.primaryKey && (
                    <Form.Text muted className="d-block">
                      <T text="A primary key field never accepts nulls." />
                    </Form.Text>
                  )}
                  {isEdit && !form.primaryKey && form.wasNullable && !form.nullable && (
                    <Form.Text muted className="d-block">
                      <T text="Saving fails if any row has no value in this field." />
                    </Form.Text>
                  )}

                  {defaultEditable(form) && (
                    <Form.Group className="mt-2 mb-1" controlId="fieldDefault">
                      <Form.Label><T text="Default value" /></Form.Label>
                      <Form.Control
                        className="font-monospace"
                        value={form.defaultExpression}
                        placeholder={t("user.id")}
                        onChange={(e) => update({ defaultExpression: e.target.value })}
                      />
                      <Form.Text muted>
                        <T text="A JavaScript expression used when a new row leaves this field empty. It can use" /> <code>user</code> <T text="(the user writing the row)," /> <code>session</code> <T text="(a guest's session) and the row's other fields. On a nullable field, an expression that gives nothing leaves it empty." />
                      </Form.Text>
                    </Form.Group>
                  )}

                  {/* The key is a field like any other (GOALS): nothing invents
                      one, so this box is how a table gets it — and, ticked on
                      more than one field, how it gets a composite one. */}
                  <Form.Check
                    className="mt-2 mb-1"
                    id="fieldPrimaryKey"
                    type="checkbox"
                    label={t("Primary key")}
                    checked={form.primaryKey}
                    onChange={(e) => update({ primaryKey: e.target.checked })}
                  />
                  <Form.Text muted className="d-block">
                    {form.primaryKey
                      ? keyValueNote(form)
                      : "Tick this on more than one field for a composite key."}
                  </Form.Text>
                </>
              )}
            </Modal.Body>
            <Modal.Footer>
              <Button variant="secondary" type="button" onClick={() => setEditing(null)}>
                <T text="Cancel" />
              </Button>
              <Button type="submit" disabled={busy || !form.name.trim() || !selected}>
                {isEdit ? "Save field" : "Add field"}
              </Button>
            </Modal.Footer>
          </Form>
        </Modal>
      </Card.Body>
    </Card>
  );
}

/**
 * The parameter form for a `Key` field (design §3.4): what it points at, and
 * what a row of the other table is shown as.
 *
 * Three selects rather than the spec-driven form, because the three settings are
 * not independent — the target and summary fields are fields *of the chosen
 * table*, and the storage type is the target field's, so it is reported rather
 * than asked. Choosing from what exists is also what makes the reference valid
 * by construction: a typed table or column name is a reference the server has to
 * refuse after the fact.
 */
function KeyFields({
  tables,
  targetFields,
  value,
  onChange,
}: {
  tables: ListTablesResponse | null;
  targetFields: ListFieldsResponse | null;
  value: KeyKind;
  onChange: (value: KeyKind) => void;
}) {
  const storage = keyStorage(value, targetFields ?? []);
  return (
    <>
      <Form.Group className="mb-2" controlId="fieldKeyTable">
        <Form.Label>
          <T text="Target table" /><span className="text-danger"> *</span>
        </Form.Label>
        <Form.Select
          value={value.target_table}
          // The other two selects are about to describe a different table, so
          // they are cleared here and re-defaulted once its fields arrive.
          onChange={(e) =>
            onChange({ target_table: e.target.value, target_field: "", summary_field: "" })
          }
        >
          <option value=""><T text="Choose a table…" /></option>
          {(tables ?? []).map((t) => (
            <option key={t.name} value={t.name}>
              {t.label || t.name}
            </option>
          ))}
        </Form.Select>
      </Form.Group>

      <Form.Group className="mb-2" controlId="fieldKeyField">
        <Form.Label>
          <T text="Target field" /><span className="text-danger"> *</span>
        </Form.Label>
        <Form.Select
          value={value.target_field}
          disabled={!value.target_table || !targetFields}
          onChange={(e) => onChange({ ...value, target_field: e.target.value })}
        >
          {!value.target_table && <option value=""><T text="Choose a table first" /></option>}
          {(targetFields ?? []).map((f) => (
            <option key={f.name} value={f.name}>
              {f.name}
              {f.primary_key ? " (primary key)" : f.unique ? " (unique)" : ""}
            </option>
          ))}
        </Form.Select>
        <Form.Text muted>
          <T text="The column this key points at — it must be unique." />
          {storage && (
            <>
              {" "}
              <T
                text="Stored as {type}, to match it."
                values={{ type: <code>{storage}</code> }}
              />
            </>
          )}
        </Form.Text>
      </Form.Group>

      <Form.Group className="mb-3" controlId="fieldKeySummary">
        <Form.Label><T text="Summary field" /></Form.Label>
        <Form.Select
          value={value.summary_field}
          disabled={!value.target_table || !targetFields}
          onChange={(e) => onChange({ ...value, summary_field: e.target.value })}
        >
          <option value="">—</option>
          {(targetFields ?? []).map((f) => (
            <option key={f.name} value={f.name}>
              {f.name}
            </option>
          ))}
        </Form.Select>
        <Form.Text muted><T text="How a referenced row is shown. Optional." /></Form.Text>
      </Form.Group>
    </>
  );
}
