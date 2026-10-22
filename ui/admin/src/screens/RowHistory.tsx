// Row history (design §14.12, TODO-history H2.3): one row's versions in the row
// form's History tab, and the pieces the Deleted tab and the changeset browser
// show entries with.
//
// An entry is one changeset's net change to the row, so one save is one entry
// however many triggers followed it. Each shows who, when and from where, and
// the fields it changed with their values before and after; a field dropped
// from the table since is still shown, marked as removed.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import {
  historyTime,
  historyValue,
  opLabel,
  opVariant,
  sourceLabel,
  whoLabel,
  type ChangesetInfo,
  type HistoryEntry,
} from "../history";
import { T } from "../i18n";

/** Who, when and from where, with a link to the whole changeset. */
export function ChangesetLine({
  changeset,
  at,
  link = true,
}: {
  changeset: ChangesetInfo;
  at?: string;
  link?: boolean;
}) {
  return (
    <span className="text-secondary small">
      {whoLabel(changeset)} · {historyTime(at ?? changeset.at)} ·{" "}
      <Badge bg="secondary-lt">{sourceLabel(changeset.source)}</Badge>
      {changeset.description && <> · {changeset.description}</>}
      {link && (
        <>
          {" "}
          ·{" "}
          <a
            href={`#/changesets/${encodeURIComponent(changeset.id)}`}
            onClick={(e) => {
              e.preventDefault();
              navigate(`/changesets/${encodeURIComponent(changeset.id)}`);
            }}
          >
            <T text="whole changeset" />
          </a>
        </>
      )}
    </span>
  );
}

/** The fields an entry changed: before and after, field by field. An insert
 * has no before and a delete no after. */
export function EntryChanges({ entry }: { entry: HistoryEntry }) {
  return (
    <Table size="sm" className="mb-0 history-changes">
      <thead>
        <tr>
          <th><T text="Field" /></th>
          {entry.op !== "i" && <th><T text="Before" /></th>}
          {entry.op !== "d" && <th><T text="After" /></th>}
        </tr>
      </thead>
      <tbody>
        {entry.changes.map((change) => (
          <tr key={change.field}>
            <td>
              <code>{change.field}</code>
              {change.removed && (
                <Badge bg="warning-lt" className="ms-1">
                  <T text="removed" />
                </Badge>
              )}
            </td>
            {entry.op !== "i" && <td className="text-secondary">{historyValue(change.old)}</td>}
            {entry.op !== "d" && <td>{historyValue(change.new)}</td>}
          </tr>
        ))}
      </tbody>
    </Table>
  );
}

/** One entry as a card-like block: what it did, who, when, and the fields. */
export function EntryBlock({
  entry,
  title,
  showChangeset = true,
}: {
  entry: HistoryEntry;
  title?: string;
  /** Who and when; left out where the changeset is the page. */
  showChangeset?: boolean;
}) {
  return (
    <div className="border rounded p-2 mb-2">
      <div className="d-flex flex-wrap align-items-center gap-2 mb-1">
        <Badge bg={opVariant(entry.op)}>{opLabel(entry.op)}</Badge>
        {title && <strong>{title}</strong>}
        {showChangeset && <ChangesetLine changeset={entry.changeset} at={entry.at} />}
      </div>
      <EntryChanges entry={entry} />
    </div>
  );
}

/** A row's history, newest first: the row form's History tab. */
export function RowHistory({ table, id }: { table: string; id: string }) {
  const [entries, setEntries] = useState<HistoryEntry[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setEntries(null);
    setError(null);
    api
      .getRowHistory(table, id)
      .then((found) => {
        if (live) setEntries(found);
      })
      .catch((err) => {
        if (live) setError(errorMessage(err, "Could not load the row's history."));
      });
    return () => {
      live = false;
    };
  }, [table, id]);

  if (error) return <Alert variant="danger">{error}</Alert>;
  if (entries === null) return <p className="text-muted mb-0"><T text="Loading…" /></p>;
  if (entries.length === 0)
    return (
      <p className="text-muted mb-0">
        <T text="No changes have been recorded for this row since history was switched on." />
      </p>
    );
  return (
    <>
      {entries.map((entry) => (
        <EntryBlock key={entry.seq} entry={entry} title={`v${entry.version}`} />
      ))}
    </>
  );
}
