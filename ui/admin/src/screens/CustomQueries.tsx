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
// The model is `../customQueries`; this file is controls. In particular nothing
// here decides whether a query is valid — the server does, with the same call a
// save makes.
//
// **The authority note is on the screen, not only in the docs** (decision 6).
// Raw SQL does not go through the row layer, so ownership formulae, rich-type
// coercion and File-field rules do not reach it. An admin opening this hole
// should be told what it is a hole in, where they are opening it.

import { useEffect, useState } from "react";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";

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
import { PublicBadge } from "../layout";
import { isPublic, roleOptions, useRoles } from "../roles";
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
      <Card.Header className="d-flex justify-content-between align-items-center">
        <span><T text="Custom queries" /></span>
        <Button
          size="sm"
          variant="outline-primary"
          onClick={() => onChange([...queries, blankQueryRow()])}
        >
          <T text="Add query" />
        </Button>
      </Card.Header>
      <Card.Body>
        <p className="text-muted small">
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
        </p>
        {queries.length === 0 && <div className="text-muted">None.</div>}
        {queries.map((query, index) => (
          <div key={index} className={index > 0 ? "border-top pt-3 mt-3" : undefined}>
            <Row className="mb-2 align-items-end">
              <Col md={3}>
                <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-name`}>
                  <T text="Name" />
                </Form.Label>
                <Form.Control
                  id={`${idPrefix}-q${index}-name`}
                  value={query.name}
                  placeholder={t("topAuthors")}
                  onChange={(e) => setQuery(index, { ...query, name: e.target.value })}
                />
              </Col>
              <Col md={2}>
                <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-language`}>
                  <T text="Language" />
                </Form.Label>
                <Form.Select
                  id={`${idPrefix}-q${index}-language`}
                  value={query.language}
                  onChange={(e) => {
                    // Columns described for the old language are not this
                    // query's any more, and neither is its check.
                    setQuery(index, {
                      ...query,
                      language: e.target.value as QueryLanguage,
                      columns: [],
                    });
                    setStatus((s) => ({ ...s, [index]: { kind: "idle" } }));
                  }}
                >
                  {languageOptions(query.language, python).map((l) => (
                    <option key={l} value={l}>
                      {languageLabel(l)}
                    </option>
                  ))}
                </Form.Select>
              </Col>
              <Col md={2}>
                <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-method`}>
                  <T text="Method" />
                </Form.Label>
                <Form.Select
                  id={`${idPrefix}-q${index}-method`}
                  value={query.method}
                  onChange={(e) => setQuery(index, { ...query, method: e.target.value })}
                >
                  {QUERY_METHODS.map((m) => (
                    <option key={m} value={m}>
                      {m}
                    </option>
                  ))}
                </Form.Select>
              </Col>
              <Col md={3}>
                <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-path`}>
                  Sub-path
                </Form.Label>
                <Form.Control
                  id={`${idPrefix}-q${index}-path`}
                  value={query.path}
                  placeholder="/reports/top-authors"
                  onChange={(e) => setQuery(index, { ...query, path: e.target.value })}
                />
              </Col>
              <Col md={2}>
                <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-role`}>
                  <T text="Minimum role" />
                  {isPublic(query.minRole) && <PublicBadge className="ms-2" />}
                </Form.Label>
                <Form.Select
                  id={`${idPrefix}-q${index}-role`}
                  value={String(query.minRole)}
                  onChange={(e) =>
                    setQuery(index, { ...query, minRole: Number(e.target.value) })
                  }
                >
                  {roleOptions(query.minRole, roles).map((r) => (
                    <option key={r.role} value={r.role}>
                      {r.name} ({r.role})
                    </option>
                  ))}
                </Form.Select>
              </Col>
            </Row>

            <Form.Group className="mb-2">
              <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-desc`}>
                <T text="Description" />
              </Form.Label>
              <Form.Control
                id={`${idPrefix}-q${index}-desc`}
                value={query.description}
                placeholder={t("What this query is for — it becomes the client method's doc comment.")}
                onChange={(e) => setQuery(index, { ...query, description: e.target.value })}
              />
            </Form.Group>

            {responseTypeApplies(query) && (
              <Row className="g-2 mb-2">
                {bodyTypeApplies(query) && (
                  <Col md={4}>
                    <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-body-type`}>
                      <T text="Body" />
                    </Form.Label>
                    <Form.Select
                      id={`${idPrefix}-q${index}-body-type`}
                      value={query.bodyType}
                      onChange={(e) =>
                        setQuery(index, { ...query, bodyType: e.target.value as PayloadType })
                      }
                    >
                      <option value="json">{t("JSON object")}</option>
                      <option value="bytes">{t("Raw bytes (Uint8Array)")}</option>
                    </Form.Select>
                  </Col>
                )}
                <Col md={4}>
                  <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-response-type`}>
                    <T text="Response" />
                  </Form.Label>
                  <Form.Select
                    id={`${idPrefix}-q${index}-response-type`}
                    value={query.responseType}
                    onChange={(e) =>
                      setQuery(index, { ...query, responseType: e.target.value as ResponseKind })
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
                </Col>
              </Row>
            )}

            {returnsApplies(query) && (
              <Form.Group className="mb-2">
                <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-returns`}>
                  <T text="Returns" />
                </Form.Label>
                <Form.Control
                  as="textarea"
                  rows={3}
                  className="font-monospace small"
                  id={`${idPrefix}-q${index}-returns`}
                  value={query.returns}
                  isInvalid={returnsError(query.returns) !== null}
                  placeholder={'{"object": {"id": "uuid", "tags": {"array": "string"}}}'}
                  onChange={(e) => setQuery(index, { ...query, returns: e.target.value })}
                />
                <Form.Control.Feedback type="invalid">
                  {returnsError(query.returns)}
                </Form.Control.Feedback>
                <Form.Text muted>
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
                </Form.Text>
              </Form.Group>
            )}

            <Row className="g-2 mb-2 align-items-end">
              <Col md={4}>
                <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-rate`}>
                  <T text="Rate limit" />
                </Form.Label>
                <Form.Select
                  id={`${idPrefix}-q${index}-rate`}
                  value={query.rateLimit}
                  onChange={(e) =>
                    setQuery(index, { ...query, rateLimit: e.target.value as RateLimitMode })
                  }
                >
                  <option value="default">
                    {isCode(query.language) && isPublic(query.minRole)
                      ? t("Default ({rate} per IP)", { rate: DEFAULT_PUBLIC_RATE })
                      : t("Default (none)")}
                  </option>
                  <option value="custom">{t("Custom")}</option>
                  <option value="none">{t("No limit")}</option>
                </Form.Select>
              </Col>
              {query.rateLimit === "custom" && (
                <>
                  <Col md={3}>
                    <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-rate-ip`}>
                      <T text="Per IP" />
                    </Form.Label>
                    <Form.Control
                      id={`${idPrefix}-q${index}-rate-ip`}
                      value={query.ratePerIp}
                      placeholder="20/min"
                      onChange={(e) => setQuery(index, { ...query, ratePerIp: e.target.value })}
                    />
                  </Col>
                  <Col md={3}>
                    <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-rate-user`}>
                      <T text="Per user" />
                    </Form.Label>
                    <Form.Control
                      id={`${idPrefix}-q${index}-rate-user`}
                      value={query.ratePerUser}
                      placeholder="120/min"
                      onChange={(e) => setQuery(index, { ...query, ratePerUser: e.target.value })}
                    />
                  </Col>
                </>
              )}
            </Row>

            {isCode(query.language) ? (
              <Form.Group className="mb-2">
                <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-code`}>
                  <T text="Code" />
                </Form.Label>
                {/* Keyed by language: the editor is built once, with its
                  grammar, so a change of language is a new editor. */}
                <CodeEditor
                  key={query.language}
                  id={`${idPrefix}-q${index}-code`}
                  language={query.language}
                  scope={{
                    request: true,
                    bytes: query.bodyType === "bytes" && bodyTypeApplies(query),
                  }}
                  value={query.code}
                  onChange={(code) => setQuery(index, { ...query, code })}
                />
                <Form.Text muted>
                  <T
                    text="The body of a function. The request is {body} (its JSON body) and {query} (its query string), the caller is {user}, and what it returns is the response."
                    values={{
                      body: <code>body</code>,
                      query: <code>query</code>,
                      user: <code>user</code>,
                    }}
                  />
                </Form.Text>
              </Form.Group>
            ) : (
              <Form.Group className="mb-2">
                <Form.Label className="small mb-1" htmlFor={`${idPrefix}-q${index}-code`}>
                  <T text="SQL" />
                </Form.Label>
                <Form.Control
                  as="textarea"
                  rows={5}
                  className="font-monospace"
                  id={`${idPrefix}-q${index}-code`}
                  value={query.code}
                  placeholder={t("select author, count(*) as n from books where year > :since group by author")}
                  onChange={(e) => setQuery(index, { ...query, code: e.target.value })}
                />
                <Form.Text muted>
                  <T text="One statement. Write a parameter as" /> <code>:name</code> <T text="and declare it below; arguments are always bound, never pasted into the text." />
                </Form.Text>
              </Form.Group>
            )}

            <div className="mb-2">
              <div className="d-flex justify-content-between align-items-center mb-1">
                <span className="small fw-semibold"><T text="Parameters" /></span>
                <Button
                  size="sm"
                  variant="outline-secondary"
                  onClick={() =>
                    setQuery(index, { ...query, params: [...query.params, blankParamRow()] })
                  }
                >
                  <T text="Add parameter" />
                </Button>
              </div>
              {query.params.length === 0 && !needsParamsNote(query) && (
                <div className="text-muted small">None.</div>
              )}
              {needsParamsNote(query) && (
                <div className="text-muted small" data-testid="undeclared-params-note">
                  <T text="None declared. The client method still sends its arguments, typed as an open object, and the code reads all of them — but nothing is typed or checked. Declare the parameters the code reads to type the client method and have each converted before the body runs." />
                </div>
              )}
              {query.params.map((param, pi) => (
                <Row key={pi} className="mb-2 align-items-end">
                  <Col md={5}>
                    <Form.Control
                      aria-label={t("Parameter name")}
                      value={param.name}
                      placeholder={t("since")}
                      onChange={(e) =>
                        setQuery(index, {
                          ...query,
                          params: query.params.map((p, i) =>
                            i === pi ? { ...p, name: e.target.value } : p,
                          ),
                        })
                      }
                    />
                  </Col>
                  <Col md={3}>
                    <Form.Select
                      aria-label={t("Parameter type")}
                      value={param.type}
                      onChange={(e) =>
                        setQuery(index, {
                          ...query,
                          params: query.params.map((p, i) =>
                            i === pi ? { ...p, type: e.target.value } : p,
                          ),
                        })
                      }
                    >
                      {PARAM_TYPES.map((t) => (
                        <option key={t} value={t}>
                          {t}
                        </option>
                      ))}
                    </Form.Select>
                  </Col>
                  <Col md={2}>
                    <Form.Check
                      type="checkbox"
                      id={`${idPrefix}-q${index}-p${pi}-req`}
                      label={t("Required")}
                      checked={param.required}
                      onChange={(e) =>
                        setQuery(index, {
                          ...query,
                          params: query.params.map((p, i) =>
                            i === pi ? { ...p, required: e.target.checked } : p,
                          ),
                        })
                      }
                    />
                  </Col>
                  <Col xs="auto">
                    <Button
                      size="sm"
                      variant="outline-danger"
                      onClick={() =>
                        setQuery(index, {
                          ...query,
                          params: query.params.filter((_, i) => i !== pi),
                        })
                      }
                    >
                      <T text="Remove" />
                    </Button>
                  </Col>
                </Row>
              ))}
              <Form.Text muted>
                {isCode(query.language) ? (
                  <T text="A declared parameter is checked and converted to its type before the body runs." />
                ) : (
                  <>
                    <T text="An optional parameter binds SQL" /> <code>NULL</code> <T text="when it is left out, which is what makes" /> <code>(:q is null or name = :q)</code> <T text="an optional filter." />
                  </>
                )}
              </Form.Text>
            </div>

            <div className="d-flex gap-2 align-items-center">
              <Button
                size="sm"
                variant="outline-primary"
                disabled={status[index]?.kind === "checking"}
                onClick={() => void check(index)}
              >
                <T text="Check" />
              </Button>
              <Button
                size="sm"
                variant="outline-danger"
                onClick={() => {
                  onChange(queries.filter((_, i) => i !== index));
                  setStatus({});
                }}
              >
                <T text="Remove query" />
              </Button>
              <CheckResult status={status[index] ?? { kind: "idle" }} query={query} />
            </div>
          </div>
        ))}
      </Card.Body>
    </Card>
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
