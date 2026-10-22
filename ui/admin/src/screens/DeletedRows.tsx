// The Deleted tab of a table's data (design §14.12, TODO-history H2.3): the
// rows deleted from a table that keeps history and not put back since, newest
// first, each as it was when deleted.
//
// Read-only for now: restoring one is H3, and goes through the row layer as
// the admin like any other write.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { ListDeletedRowsResponse } from "../client";
import { historyValue } from "../history";
import { T } from "../i18n";
import { ChangesetLine } from "./RowHistory";

const PAGE = 50;

export function DeletedRows({ table }: { table: string }) {
  const [rows, setRows] = useState<ListDeletedRowsResponse | null>(null);
  const [offset, setOffset] = useState(0);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setError(null);
    api
      .listDeletedRows(table, { limit: PAGE, offset })
      .then((found) => {
        if (live) setRows(found);
      })
      .catch((err) => {
        if (live) setError(errorMessage(err, "Could not load the deleted rows."));
      });
    return () => {
      live = false;
    };
  }, [table, offset]);

  if (error) return <Alert variant="danger">{error}</Alert>;
  if (rows === null) return <p className="text-muted"><T text="Loading…" /></p>;
  if (rows.length === 0 && offset === 0)
    return (
      <Alert variant="secondary">
        <T text="No rows have been deleted since history was switched on, or every one has been put back." />
      </Alert>
    );

  return (
    <>
      {rows.map((deleted) => {
        const values = (deleted.row ?? {}) as Record<string, unknown>;
        const removed = (deleted.removed ?? {}) as Record<string, unknown>;
        return (
          <div className="card mb-2" key={`${deleted.key}-${deleted.seq}`}>
            <div className="card-body p-2">
              <div className="d-flex flex-wrap align-items-center gap-2 mb-1">
                <strong>
                  <T text="Row" /> <code>{deleted.key}</code>
                </strong>
                <ChangesetLine changeset={deleted.changeset} at={deleted.at} />
              </div>
              <Table size="sm" className="mb-0">
                <tbody>
                  {Object.entries(values).map(([field, value]) => (
                    <tr key={field}>
                      <td className="w-25">
                        <code>{field}</code>
                      </td>
                      <td>{historyValue(value)}</td>
                    </tr>
                  ))}
                  {Object.entries(removed).map(([field, value]) => (
                    <tr key={`removed-${field}`}>
                      <td className="w-25">
                        <code>{field}</code>
                        <Badge bg="warning-lt" className="ms-1">
                          <T text="removed" />
                        </Badge>
                      </td>
                      <td className="text-secondary">{historyValue(value)}</td>
                    </tr>
                  ))}
                </tbody>
              </Table>
            </div>
          </div>
        );
      })}
      <div className="d-flex gap-2">
        <Button
          variant="outline-secondary"
          size="sm"
          disabled={offset === 0}
          onClick={() => setOffset(Math.max(0, offset - PAGE))}
        >
          <T text="Newer" />
        </Button>
        <Button
          variant="outline-secondary"
          size="sm"
          disabled={rows.length < PAGE}
          onClick={() => setOffset(offset + PAGE)}
        >
          <T text="Older" />
        </Button>
      </div>
    </>
  );
}
