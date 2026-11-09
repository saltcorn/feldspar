// The **custom query** editor: an application's own endpoints, written as SQL,
// JavaScript or Python (§13.4).
//
// One list per API row that serves them, each query carrying a name, a
// language, a method, a sub-path, a role floor, the source and its declared
// parameters. The button that matters is **Check**: for SQL it prepares the
// statement on the server and shows the columns Postgres says it returns —
// which is both the validation ("column `titel` does not exist", in Postgres's
// own words, while its author is still looking at the SQL) and the
// *documentation*, because those columns are what the generated client method
// will hand back. A JavaScript or Python body is checked for everything but the
// body itself, which only runs when it is called; its method returns JSON.
//
// Python is offered only where the server can run it (`getPythonStatus`).
//
// The card lists the queries as a table: name, endpoint, language, role and
// parameters, one row each. A row opens that query's editor in the card's
// place, with a way back to the list, so one query is inspected at a time
// rather than every query's every field stacked on the page.
//
// The model is `../customQueries`; this file is controls. In particular nothing
// here decides whether a query is valid — the server does, with the same call a
// save makes.
//
// **The authority note is on the screen, not only in the docs** (decision 6).
// Raw SQL does not go through the row layer, so ownership formulae, rich-type
// coercion and File-field rules do not reach it. An admin opening this hole
// should be told what it is a hole in, where they are opening it.

import { useEffect, useState, type ReactNode } from "react";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { CodeEditor } from "../CodeEditor";
import {
  PARAM_TYPES,
  QUERY_METHODS,
  blankParamRow,
  blankQueryRow,
  bodyTypeApplies,
  describeBody,
  isCode,
  languageLabel,
  languageOptions,
  languageBadgeClass,
  methodTone,
  needsParamsNote,
  pythonAvailable,
  responseTypeApplies,
  returnsApplies,
  returnsError,
  DEFAULT_PUBLIC_RATE,
  statusSummary,
  type CheckStatus,
  type PayloadType,
  type ResponseKind,
  type QueryLanguage,
  type RateLimitMode,
  type QueryRow,
} from "../customQueries";
import { IconArrowLeft, IconChevronRight, IconPlus, IconTrash } from "../icons";
import { AlertBody, PublicBadge, StatusBadge } from "../layout";
import { isPublic, roleOptions, useRoles, type Roles } from "../roles";
import { T, useT } from "../i18n";

/** The queries of one API row, edited in place.
 *
 * `tables` is the application's declared table subset, sent with a check so the
 * server can also refuse a name or a path that this app's own table routes
 * already answer — the whole refusal a save would give, from the check button. */
export function CustomQueries({
  queries,
  tables,
  idPrefix,
  onChange,
}: {
  queries: QueryRow[];
  tables: string[];
  idPrefix: string;
  onChange: (queries: QueryRow[]) => void;
}) {
  const { t } = useT();
  const roles = useRoles();
  // Keyed by index rather than held on the row: a check result is about this
  // editing session, not part of what gets saved.
  const [status, setStatus] = useState<Record<number, CheckStatus>>({});
  // The query whose editor is open, by index; `null` shows the list.
  const [open, setOpen] = useState<number | null>(null);
  // Whether Python is on offer. Asked once; a server that cannot say is one
  // that is not offered it.
  const [python, setPython] = useState(false);
  useEffect(() => {
    let cancelled = false;
    api
      .getPythonStatus()
      .then((s) => {
        if (!cancelled) setPython(pythonAvailable(s.state));
      })
      .catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, []);

  const setQuery = (index: number, next: QueryRow) =>
    onChange(queries.map((q, i) => (i === index ? next : q)));

  const check = async (index: number) => {
    setStatus((s) => ({ ...s, [index]: { kind: "checking" } }));
    try {
      const { columns } = await api.describeCustomQuery(describeBody(queries[index], tables));
      // The described columns land on the row as well as in the status: they
      // are what the stored query will carry, and showing them after a reopen
      // is the same fact as showing them now.
      setQuery(index, { ...queries[index], columns });
      setStatus((s) => ({
        ...s,
        [index]: isCode(queries[index].language)
          ? { kind: "checked", typed: returnsApplies(queries[index]) && !!queries[index].returns.trim() }
          : { kind: "ok", columns },
      }));
    } catch (err) {
      setStatus((s) => ({
        ...s,
        [index]: {
          kind: "error",
          message: errorMessage(err, "The server could not prepare this query."),
        },
      }));
    }
  };

  return (
    <Card className="mb-3">
      <Card.Header className="d-flex justify-content-between align-items-center gap-2">
        {open !== null && queries[open] ? (
          <>
            <span className="d-flex align-items-center gap-3 text-truncate">
              <Button size="sm" variant="ghost-secondary" onClick={() => setOpen(null)}>
                <IconArrowLeft className="icon-2" />
                <T text="All queries" />
              </Button>
              <span className="d-flex flex-column text-truncate">
                <span className="subheader">
                  <T text="Custom query" />
                </span>
                <span className="fw-semibold text-truncate">
                  {queries[open].name.trim() || t("New query")}
                </span>
              </span>
            </span>
            <Endpoint query={queries[open]} />
          </>
        ) : (
          <>
            <Card.Title className="mb-0 d-flex align-items-center gap-2">
              <T text="Custom queries" />
              {queries.length > 0 && (
                <span className="badge bg-secondary-lt">{queries.length}</span>
              )}
            </Card.Title>
            <Button
              size="sm"
              variant="primary"
              onClick={() => {
                onChange([...queries, blankQueryRow()]);
                setOpen(queries.length);
              }}
            >
              <IconPlus className="icon-2" />
              <T text="Add query" />
            </Button>
          </>
        )}
      </Card.Header>
      <Card.Body>
        {open === null && (
          <div className="alert alert-secondary small mb-3">
            <AlertBody>
            <T text="Each query becomes one endpoint on this API and one typed method on the app’s generated client. A SQL query’s return type is taken from the columns Postgres reports; a JavaScript or Python query returns whatever its body returns." />{" "}
            {/* Two sentences, each whole, each with its emphasis as a hole: a
              translator can move the emphasised clause, which is the thing three
              fragments would have made impossible. */}
            <T
              text="{claim} raw SQL does not go through the row layer, so ownership formulae, rich-type coercion and File-field rules do not apply to it, and a write inside one raises no table event."
              values={{
                claim: (
                  <strong>
                    <T text="A custom query’s authority is its own:" />
                  </strong>
                ),
              }}
            />{" "}
            <T
              text="What still applies is the role floor below — {default} — and the caller’s database context, so a row-level-security policy still decides what the query can see."
              values={{
                default: (
                  <em>
                    <T text="admin unless you say otherwise" />
                  </em>
                ),
              }}
            />{" "}
            <T
              text="A JavaScript or Python query runs as a trigger’s code does, with the administrator’s authority over the tables unless it asks for the caller’s with {asUser}."
              values={{ asUser: <code>db.asUser()</code> }}
            />
            </AlertBody>
          </div>
        )}
        {open !== null && queries[open] ? (
          <QueryEditor
            query={queries[open]}
            id={`${idPrefix}-q${open}`}
            python={python}
            roles={roles}
            status={status[open] ?? { kind: "idle" }}
            onChange={(next) => setQuery(open, next)}
            onCheck={() => void check(open)}
            onResetCheck={() => setStatus((s) => ({ ...s, [open]: { kind: "idle" } }))}
            onRemove={() => {
              onChange(queries.filter((_, i) => i !== open));
              setStatus({});
              setOpen(null);
            }}
          />
        ) : (
          <QueryList
            queries={queries}
            roles={roles}
            status={status}
            onOpen={setOpen}
          />
        )}
      </Card.Body>
    </Card>
  );
}

/** A method as a coloured badge, beside the path it answers on. */
function Endpoint({ query }: { query: Pick<QueryRow, "method" | "path"> }) {
  return (
    <span className="d-inline-flex align-items-center gap-2 text-nowrap">
      <StatusBadge tone={methodTone(query.method)} className="font-monospace">
        {query.method}
      </StatusBadge>
      <code className="p-0 bg-transparent text-reset">{query.path.trim() || "/"}</code>
    </span>
  );
}

/** The queries as a table, one row each; a row opens its query's editor. */
export function QueryList({
  queries,
  roles,
  status,
  onOpen,
}: {
  queries: QueryRow[];
  roles: Roles;
  status: Record<number, CheckStatus>;
  onOpen: (index: number) => void;
}) {
  const { t } = useT();
  if (queries.length === 0) {
    return (
      <div className="text-center text-secondary py-4 border rounded">
        <T text="None." />
      </div>
    );
  }
  const roleName = (role: number) =>
    roleOptions(role, roles).find((r) => r.role === role)?.name ?? String(role);
  return (
    <div className="border rounded">
      <Table hover responsive className="card-table table-vcenter mb-0 query-list">
        <thead>
          <tr>
            <th><T text="Name" /></th>
            <th><T text="Endpoint" /></th>
            <th><T text="Language" /></th>
            <th><T text="Minimum role" /></th>
            <th><T text="Parameters" /></th>
            <th className="w-1" aria-label={t("Open")} />
          </tr>
        </thead>
        <tbody>
          {queries.map((query, index) => {
            const params = query.params.filter((p) => p.name.trim());
            return (
              <tr key={index} role="button" onClick={() => onOpen(index)}>
                <td className="query-list-name">
                  <div className="fw-semibold">{query.name.trim() || t("New query")}</div>
                  {query.description.trim() && (
                    <div className="query-list-description text-secondary small" title={query.description}>
                      {query.description}
                    </div>
                  )}
                  {status[index]?.kind === "error" && (
                    <div className="small text-danger">{statusSummary(status[index])}</div>
                  )}
                </td>
                <td>
                  <Endpoint query={query} />
                </td>
                <td>
                  <span className={`badge ${languageBadgeClass(query.language)}`}>
                    {languageLabel(query.language)}
                  </span>
                </td>
                <td className="text-nowrap">
                  {isPublic(query.minRole) ? (
                    <PublicBadge />
                  ) : (
                    <StatusBadge tone="secondary">{roleName(query.minRole)}</StatusBadge>
                  )}
                </td>
                <td>
                  {params.length === 0 ? (
                    <span className="text-secondary">—</span>
                  ) : (
                    <span className="d-flex flex-wrap gap-1">
                      {params.map((p, pi) => (
                        <span
                          key={pi}
                          className={`badge badge-outline font-monospace fw-normal ${p.required ? "text-reset" : "text-secondary"}`}
                          title={`${p.type}${p.required ? "" : ` (${t("optional")})`}`}
                        >
                          {p.required ? p.name.trim() : `${p.name.trim()}?`}
                        </span>
                      ))}
                    </span>
                  )}
                </td>
                <td className="text-end">
                  <IconChevronRight className="icon-2 text-secondary" />
                </td>
              </tr>
            );
          })}
        </tbody>
      </Table>
    </div>
  );
}

/** A framed part of the editor: a header bar with its title and actions, and
 * its body. */
function Panel({
  title,
  actions,
  flush,
  className,
  children,
}: {
  title: ReactNode;
  actions?: ReactNode;
  /** The body has no padding of its own: a code editor or a table fills it. */
  flush?: boolean;
  className?: string;
  children: ReactNode;
}) {
  return (
    <section className={`query-panel${className ? ` ${className}` : ""}`}>
      <header className="query-panel-header">
        <span className="subheader">{title}</span>
        {actions && <span className="d-flex align-items-center gap-2">{actions}</span>}
      </header>
      <div className={flush ? "query-panel-body" : "query-panel-body p-3"}>{children}</div>
    </section>
  );
}

/** One query's editor: the sub view a row of the list opens.
 *
 * Two columns: on the left what the query *is* — its request line, name,
 * description, code, parameters and declared answer; on the right a settings
 * panel for how it is served — language, role, payloads and rate limit. */
export function QueryEditor({
  query,
  id,
  python,
  roles,
  status,
  onChange,
  onCheck,
  onResetCheck,
  onRemove,
}: {
  query: QueryRow;
  /** The prefix of every control's id, unique on the page. */
  id: string;
  python: boolean;
  roles: Roles;
  status: CheckStatus;
  onChange: (query: QueryRow) => void;
  onCheck: () => void;
  onResetCheck: () => void;
  onRemove: () => void;
}) {
  const { t } = useT();
  const update = onChange;
  const setParam = (pi: number, change: Partial<QueryRow["params"][number]>) =>
    update({
      ...query,
      params: query.params.map((p, i) => (i === pi ? { ...p, ...change } : p)),
    });
  return (
    <div>
      <Row className="g-4">
        <Col lg={8}>
          {/* The request line: method and path as one control, the way an
            HTTP client shows a request. */}
          <Form.Label className="visually-hidden" htmlFor={`${id}-method`}>
            <T text="Method" />
          </Form.Label>
          <Form.Label className="visually-hidden" htmlFor={`${id}-path`}>
            <T text="Path" />
          </Form.Label>
          <div className="input-group input-group-flat query-request-line mb-3">
            <Form.Select
              id={`${id}-method`}
              className={`query-method text-${methodTone(query.method)} fw-bold font-monospace`}
              value={query.method}
              onChange={(e) => update({ ...query, method: e.target.value })}
            >
              {QUERY_METHODS.map((m) => (
                <option key={m} value={m}>
                  {m}
                </option>
              ))}
            </Form.Select>
            <Form.Control
              id={`${id}-path`}
              className="font-monospace"
              value={query.path}
              placeholder="/reports/top-authors"
              onChange={(e) => update({ ...query, path: e.target.value })}
            />
          </div>

          <Form.Group className="mb-3">
            <Form.Label className="small mb-1" htmlFor={`${id}-name`}>
              <T text="Name" />
            </Form.Label>
            <Form.Control
              id={`${id}-name`}
              className="font-monospace fw-semibold"
              value={query.name}
              placeholder={t("topAuthors")}
              onChange={(e) => update({ ...query, name: e.target.value })}
            />
          </Form.Group>
          <Form.Group className="mb-4">
            <Form.Label className="small mb-1" htmlFor={`${id}-desc`}>
              <T text="Description" />
            </Form.Label>
            <Form.Control
              as="textarea"
              rows={Math.min(5, Math.max(2, Math.ceil(query.description.length / 90)))}
              id={`${id}-desc`}
              value={query.description}
              placeholder={t("What this query is for — it becomes the client method's doc comment.")}
              onChange={(e) => update({ ...query, description: e.target.value })}
            />
          </Form.Group>

          <Panel
            title={isCode(query.language) ? <T text="Code" /> : <T text="SQL" />}
            actions={
              <span className={`badge ${languageBadgeClass(query.language)}`}>
                {languageLabel(query.language)}
              </span>
            }
            flush
            className="mb-4"
          >
            {isCode(query.language) ? (
              <>
                <Form.Label className="visually-hidden" htmlFor={`${id}-code`}>
                  <T text="Code" />
                </Form.Label>
                {/* Keyed by language: the editor is built once, with its
                  grammar, so a change of language is a new editor. */}
                <CodeEditor
                  key={query.language}
                  id={`${id}-code`}
                  language={query.language}
                  scope={{
                    request: true,
                    bytes: query.bodyType === "bytes" && bodyTypeApplies(query),
                  }}
                  value={query.code}
                  onChange={(code) => update({ ...query, code })}
                />
                <div className="query-panel-footer">
                  <T
                    text="The body of a function. The request is {body} (its JSON body) and {query} (its query string), the caller is {user}, and what it returns is the response."
                    values={{
                      body: <code>body</code>,
                      query: <code>query</code>,
                      user: <code>user</code>,
                    }}
                  />
                </div>
              </>
            ) : (
              <>
                <Form.Label className="visually-hidden" htmlFor={`${id}-code`}>
                  <T text="SQL" />
                </Form.Label>
                <Form.Control
                  as="textarea"
                  rows={Math.min(16, Math.max(6, query.code.split("\n").length + 1))}
                  className="font-monospace border-0 rounded-0 shadow-none query-sql"
                  id={`${id}-code`}
                  value={query.code}
                  placeholder={t("select author, count(*) as n from books where year > :since group by author")}
                  onChange={(e) => update({ ...query, code: e.target.value })}
                />
                <div className="query-panel-footer">
                  <T text="One statement. Write a parameter as" /> <code>:name</code> <T text="and declare it below; arguments are always bound, never pasted into the text." />
                </div>
              </>
            )}
          </Panel>

          <Panel
            title={
              <>
                <T text="Parameters" />
                {query.params.length > 0 && (
                  <span className="badge bg-secondary-lt ms-2">{query.params.length}</span>
                )}
              </>
            }
            actions={
              <Button
                size="sm"
                variant="ghost-primary"
                onClick={() => update({ ...query, params: [...query.params, blankParamRow()] })}
              >
                <IconPlus className="icon-2" />
                <T text="Add parameter" />
              </Button>
            }
            flush
            className="mb-4"
          >
            {query.params.length === 0 ? (
              <div className="p-3 text-secondary small">
                {needsParamsNote(query) ? (
                  <span data-testid="undeclared-params-note">
                    <T text="None declared. The client method still sends its arguments, typed as an open object, and the code reads all of them — but nothing is typed or checked. Declare the parameters the code reads to type the client method and have each converted before the body runs." />
                  </span>
                ) : (
                  <T text="None." />
                )}
              </div>
            ) : (
              <Table className="card-table table-vcenter mb-0 query-params">
                <thead>
                  <tr>
                    <th><T text="Name" /></th>
                    <th className="w-25"><T text="Type" /></th>
                    <th className="w-1"><T text="Required" /></th>
                    <th className="w-1" aria-label={t("Remove")} />
                  </tr>
                </thead>
                <tbody>
                  {query.params.map((param, pi) => (
                    <tr key={pi}>
                      <td>
                        <Form.Control
                          size="sm"
                          className="font-monospace"
                          aria-label={t("Parameter name")}
                          value={param.name}
                          placeholder={t("since")}
                          onChange={(e) => setParam(pi, { name: e.target.value })}
                        />
                      </td>
                      <td>
                        <Form.Select
                          size="sm"
                          aria-label={t("Parameter type")}
                          value={param.type}
                          onChange={(e) => setParam(pi, { type: e.target.value })}
                        >
                          {PARAM_TYPES.map((ty) => (
                            <option key={ty} value={ty}>
                              {ty}
                            </option>
                          ))}
                        </Form.Select>
                      </td>
                      <td className="text-center">
                        <Form.Check
                          type="switch"
                          className="m-0 d-inline-block"
                          id={`${id}-p${pi}-req`}
                          aria-label={t("Required")}
                          checked={param.required}
                          onChange={(e) => setParam(pi, { required: e.target.checked })}
                        />
                      </td>
                      <td>
                        <Button
                          size="sm"
                          variant="ghost-danger"
                          className="btn-icon"
                          aria-label={t("Remove")}
                          title={t("Remove")}
                          onClick={() =>
                            update({ ...query, params: query.params.filter((_, i) => i !== pi) })
                          }
                        >
                          <IconTrash className="icon-2" />
                        </Button>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </Table>
            )}
            <div className="query-panel-footer">
              {isCode(query.language) ? (
                <T text="A declared parameter is checked and converted to its type before the body runs." />
              ) : (
                <>
                  <T text="An optional parameter binds SQL" /> <code>NULL</code> <T text="when it is left out, which is what makes" /> <code>(:q is null or name = :q)</code> <T text="an optional filter." />
                </>
              )}
            </div>
          </Panel>

          {returnsApplies(query) && (
            <Panel title={<T text="Returns" />} flush>
              <Form.Label className="visually-hidden" htmlFor={`${id}-returns`}>
                <T text="Returns" />
              </Form.Label>
              <Form.Control
                as="textarea"
                rows={Math.min(20, Math.max(3, query.returns.split("\n").length + 1))}
                className="font-monospace small border-0 rounded-0 shadow-none query-sql"
                id={`${id}-returns`}
                value={query.returns}
                isInvalid={returnsError(query.returns) !== null}
                placeholder={'{"object": {"id": "uuid", "tags": {"array": "string"}}}'}
                onChange={(e) => update({ ...query, returns: e.target.value })}
              />
              <Form.Control.Feedback type="invalid" className="px-3">
                {returnsError(query.returns)}
              </Form.Control.Feedback>
              <div className="query-panel-footer">
                <T
                  text="Optional. What the body returns, so the client method is typed instead of {unknown}: a type name ({names}) or {object}, {array}, {optional} and {union}. Checked in previews only."
                  values={{
                    unknown: <code>unknown</code>,
                    names: <code>string, int, float, bool, json, uuid, date, timestamp</code>,
                    object: <code>{'{"object": {…}}'}</code>,
                    array: <code>{'{"array": T}'}</code>,
                    optional: <code>{'{"optional": T}'}</code>,
                    union: <code>{'{"union": [T, …]}'}</code>,
                  }}
                />
              </div>
            </Panel>
          )}
        </Col>

        <Col lg={4}>
          <Panel title={<T text="Settings" />} className="query-settings">
            <Form.Group className="mb-3">
              <Form.Label className="small mb-1" htmlFor={`${id}-language`}>
                <T text="Language" />
              </Form.Label>
              <Form.Select
                id={`${id}-language`}
                value={query.language}
                onChange={(e) => {
                  // Columns described for the old language are not this
                  // query's any more, and neither is its check.
                  update({
                    ...query,
                    language: e.target.value as QueryLanguage,
                    columns: [],
                  });
                  onResetCheck();
                }}
              >
                {languageOptions(query.language, python).map((l) => (
                  <option key={l} value={l}>
                    {languageLabel(l)}
                  </option>
                ))}
              </Form.Select>
            </Form.Group>

            <Form.Group className="mb-3">
              <Form.Label className="small mb-1 d-flex align-items-center" htmlFor={`${id}-role`}>
                <T text="Minimum role" />
                {isPublic(query.minRole) && <PublicBadge className="ms-auto" />}
              </Form.Label>
              <Form.Select
                id={`${id}-role`}
                value={String(query.minRole)}
                onChange={(e) => update({ ...query, minRole: Number(e.target.value) })}
              >
                {roleOptions(query.minRole, roles).map((r) => (
                  <option key={r.role} value={r.role}>
                    {r.name} ({r.role})
                  </option>
                ))}
              </Form.Select>
            </Form.Group>

            {responseTypeApplies(query) && (
              <>
                <hr className="my-3" />
                {bodyTypeApplies(query) && (
                  <Form.Group className="mb-3">
                    <Form.Label className="small mb-1" htmlFor={`${id}-body-type`}>
                      <T text="Body" />
                    </Form.Label>
                    <Form.Select
                      id={`${id}-body-type`}
                      value={query.bodyType}
                      onChange={(e) => update({ ...query, bodyType: e.target.value as PayloadType })}
                    >
                      <option value="json">{t("JSON object")}</option>
                      <option value="bytes">{t("Raw bytes (Uint8Array)")}</option>
                    </Form.Select>
                  </Form.Group>
                )}
                <Form.Group className="mb-3">
                  <Form.Label className="small mb-1" htmlFor={`${id}-response-type`}>
                    <T text="Response" />
                  </Form.Label>
                  <Form.Select
                    id={`${id}-response-type`}
                    value={query.responseType}
                    onChange={(e) =>
                      update({ ...query, responseType: e.target.value as ResponseKind })
                    }
                  >
                    <option value="json">{t("JSON")}</option>
                    <option value="bytes">{t("Bytes or a file (Blob)")}</option>
                    <option value="stream">{t("A live stream (live.stream)")}</option>
                  </Form.Select>
                  {query.responseType === "stream" && (
                    <Form.Text className="small">
                      <T text="Opened over the app's live socket: end the code with return live.stream(name, key, { write }). The query is the access check." />
                    </Form.Text>
                  )}
                </Form.Group>
              </>
            )}

            <hr className="my-3" />
            <Form.Group className={query.rateLimit === "custom" ? "mb-3" : "mb-0"}>
              <Form.Label className="small mb-1" htmlFor={`${id}-rate`}>
                <T text="Rate limit" />
              </Form.Label>
              <Form.Select
                id={`${id}-rate`}
                value={query.rateLimit}
                onChange={(e) => update({ ...query, rateLimit: e.target.value as RateLimitMode })}
              >
                <option value="default">
                  {isCode(query.language) && isPublic(query.minRole)
                    ? t("Default ({rate} per IP)", { rate: DEFAULT_PUBLIC_RATE })
                    : t("Default (none)")}
                </option>
                <option value="custom">{t("Custom")}</option>
                <option value="none">{t("No limit")}</option>
              </Form.Select>
            </Form.Group>
            {query.rateLimit === "custom" && (
              <Row className="g-2">
                <Col>
                  <Form.Label className="small mb-1" htmlFor={`${id}-rate-ip`}>
                    <T text="Per IP" />
                  </Form.Label>
                  <Form.Control
                    id={`${id}-rate-ip`}
                    value={query.ratePerIp}
                    placeholder="20/min"
                    onChange={(e) => update({ ...query, ratePerIp: e.target.value })}
                  />
                </Col>
                <Col>
                  <Form.Label className="small mb-1" htmlFor={`${id}-rate-user`}>
                    <T text="Per user" />
                  </Form.Label>
                  <Form.Control
                    id={`${id}-rate-user`}
                    value={query.ratePerUser}
                    placeholder="120/min"
                    onChange={(e) => update({ ...query, ratePerUser: e.target.value })}
                  />
                </Col>
              </Row>
            )}
          </Panel>
        </Col>
      </Row>

      <div className="query-editor-footer">
        <Button
          variant="primary"
          disabled={status.kind === "checking"}
          onClick={onCheck}
        >
          <T text="Check" />
        </Button>
        <span className="flex-fill small">
          <CheckResult status={status} query={query} />
        </span>
        <Button variant="ghost-danger" onClick={onRemove}>
          <IconTrash className="icon-2" />
          <T text="Remove query" />
        </Button>
      </div>
    </div>
  );
}

/** What the check said — the refusal in Postgres's words, or the shape the
 * client method will return.
 *
 * With no check yet, the columns *stored* with the query are shown instead:
 * reopening an application should not make an admin re-derive what its query
 * returns, and those columns are the database's own answer from the last save. */
function CheckResult({ status, query }: { status: CheckStatus; query: QueryRow }) {
  if (status.kind === "error") {
    return <span className="text-danger small">{statusSummary(status)}</span>;
  }
  if (status.kind !== "idle") {
    const success = status.kind === "ok" || status.kind === "checked";
    return (
      <span className={success ? "text-success small" : "text-muted small"}>
        {statusSummary(status)}
      </span>
    );
  }
  if (isCode(query.language)) {
    return (
      <span className="text-muted small">
        <T text="Returns whatever its body returns." />
      </span>
    );
  }
  if (query.columns.length === 0) {
    return (
      <span className="text-muted small">
        <T text="Not checked yet — Check prepares it and shows what it returns." />
      </span>
    );
  }
  return (
    <span className="text-muted small">
      {statusSummary({ kind: "ok", columns: query.columns })}
    </span>
  );
}
