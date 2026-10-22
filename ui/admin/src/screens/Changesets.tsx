// The changeset browser (design §14.12, TODO-history H2.3): what each user
// action changed, across the tables that keep history.
//
// A changeset is one action — a save and every trigger it set off, a CSV
// import, a workflow run, one MCP call, one offline upload, one `psql`
// transaction — so this is the screen that answers "what did that do?". The
// list filters by user, table, source and date; a changeset opens to its
// entries, row by row. Undo arrives here in H3.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import type { GetChangesetResponse, ListChangesetsResponse } from "../client";
import {
  EMPTY_CHANGESET_FILTER,
  SOURCE_LABELS,
  changesetQuery,
  historyTime,
  sourceLabel,
  whoLabel,
  type ChangesetFilterForm,
} from "../history";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader } from "../layout";
import { T } from "../i18n";
import { ChangesetLine, EntryBlock } from "./RowHistory";

const PAGE = 50;

/** The list, filtered; opened with `?table=` from a table's data screen. */
export function Changesets({ initialTable }: { initialTable?: string }) {
  const [form, setForm] = useState<ChangesetFilterForm>({
    ...EMPTY_CHANGESET_FILTER,
    table: initialTable ?? "",
  });
  // What was last searched for: the boxes apply when the form is submitted.
  const [applied, setApplied] = useState<ChangesetFilterForm>(form);
  const [offset, setOffset] = useState(0);
  const [found, setFound] = useState<ListChangesetsResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setError(null);
    api
      .listChangesets(changesetQuery(applied, { limit: PAGE, offset }))
      .then((list) => {
        if (live) setFound(list);
      })
      .catch((err) => {
        if (live) setError(errorMessage(err, "Could not load the changesets."));
      });
    return () => {
      live = false;
    };
  }, [applied, offset]);

  const search = (e: FormEvent) => {
    e.preventDefault();
    setOffset(0);
    setApplied(form);
  };

  return (
    <>
      <PageHeader
        pretitle="Row history"
        title="Changesets"
        actions={
          <Button variant="outline-secondary" onClick={() => navigate("/tables")}>
            <IconArrowLeft className="icon-2" />
            <T text="Tables" />
          </Button>
        }
      />
      <PageBody>
        <Form onSubmit={search} className="row g-2 align-items-end mb-3">
          <Form.Group className="col-md-3" controlId="changesets-user">
            <Form.Label><T text="User id" /></Form.Label>
            <Form.Control
              value={form.user}
              onChange={(e) => setForm({ ...form, user: e.target.value })}
            />
          </Form.Group>
          <Form.Group className="col-md-3" controlId="changesets-table">
            <Form.Label><T text="Table" /></Form.Label>
            <Form.Control
              value={form.table}
              onChange={(e) => setForm({ ...form, table: e.target.value })}
            />
          </Form.Group>
          <Form.Group className="col-md-2" controlId="changesets-source">
            <Form.Label><T text="Source" /></Form.Label>
            <Form.Select
              value={form.source}
              onChange={(e) => setForm({ ...form, source: e.target.value })}
            >
              <option value="">—</option>
              {Object.entries(SOURCE_LABELS).map(([value, label]) => (
                <option key={value} value={value}>
                  {label}
                </option>
              ))}
            </Form.Select>
          </Form.Group>
          <Form.Group className="col-md-2" controlId="changesets-since">
            <Form.Label><T text="Since" /></Form.Label>
            <Form.Control
              type="date"
              value={form.since}
              onChange={(e) => setForm({ ...form, since: e.target.value })}
            />
          </Form.Group>
          <div className="col-md-2">
            <Button type="submit" className="w-100">
              <T text="Search" />
            </Button>
          </div>
        </Form>

        {error && <Alert variant="danger">{error}</Alert>}
        {found === null && !error && <p className="text-muted"><T text="Loading…" /></p>}
        {found !== null && found.length === 0 && (
          <Alert variant="secondary">
            <T text="No changesets match. Only changes to tables that keep row history are listed." />
          </Alert>
        )}
        {found !== null && found.length > 0 && (
          <div className="card">
            <Table hover responsive className="card-table mb-0">
              <thead>
                <tr>
                  <th><T text="When" /></th>
                  <th><T text="Who" /></th>
                  <th><T text="Source" /></th>
                  <th><T text="Tables" /></th>
                  <th className="text-end"><T text="Rows" /></th>
                </tr>
              </thead>
              <tbody>
                {found.map((changeset) => (
                  <tr
                    key={changeset.id}
                    role="button"
                    onClick={() => navigate(`/changesets/${encodeURIComponent(changeset.id)}`)}
                  >
                    <td>{historyTime(changeset.at)}</td>
                    <td>{whoLabel(changeset)}</td>
                    <td>
                      <Badge bg="secondary-lt">{sourceLabel(changeset.source)}</Badge>
                    </td>
                    <td>
                      {changeset.tables.map((table) => (
                        <code key={table} className="me-1">
                          {table}
                        </code>
                      ))}
                    </td>
                    <td className="text-end">{changeset.entries}</td>
                  </tr>
                ))}
              </tbody>
            </Table>
          </div>
        )}
        {found !== null && (offset > 0 || found.length === PAGE) && (
          <div className="d-flex gap-2 mt-2">
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
              disabled={found.length < PAGE}
              onClick={() => setOffset(offset + PAGE)}
            >
              <T text="Older" />
            </Button>
          </div>
        )}
      </PageBody>
    </>
  );
}

/** One changeset: who, when, where from, and every row it changed. */
export function ChangesetDetail({ id }: { id: string }) {
  const [changeset, setChangeset] = useState<GetChangesetResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    let live = true;
    setError(null);
    setChangeset(null);
    api
      .getChangeset(id)
      .then((found) => {
        if (live) setChangeset(found);
      })
      .catch((err) => {
        if (live) setError(errorMessage(err, "Could not load the changeset."));
      });
    return () => {
      live = false;
    };
  }, [id]);

  return (
    <>
      <PageHeader
        pretitle="Changeset"
        title={changeset ? sourceLabel(changeset.source) : "Changeset"}
        actions={
          <Button variant="outline-secondary" onClick={() => navigate("/changesets")}>
            <IconArrowLeft className="icon-2" />
            <T text="Changesets" />
          </Button>
        }
      />
      <PageBody>
        {error && <Alert variant="danger">{error}</Alert>}
        {!changeset && !error && <p className="text-muted"><T text="Loading…" /></p>}
        {changeset && (
          <>
            <p>
              <ChangesetLine changeset={changeset} link={false} />
            </p>
            {changeset.cause && (
              <p className="small">
                <T text="Caused by" />{" "}
                <a
                  href={`#/changesets/${encodeURIComponent(changeset.cause)}`}
                  onClick={(e) => {
                    e.preventDefault();
                    navigate(`/changesets/${encodeURIComponent(changeset.cause ?? "")}`);
                  }}
                >
                  <code>{changeset.cause}</code>
                </a>
              </p>
            )}
            {changeset.entries.length === 0 && (
              <Alert variant="secondary">
                <T text="This changeset changed nothing in a table that keeps row history." />
              </Alert>
            )}
            {changeset.entries.map((entry) => (
              <EntryBlock
                key={entry.seq}
                entry={entry}
                title={`${entry.table} ${entry.key}`}
                showChangeset={false}
              />
            ))}
          </>
        )}
      </PageBody>
    </>
  );
}
