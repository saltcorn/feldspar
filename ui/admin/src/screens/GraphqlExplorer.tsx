// The GraphQL explorer (design §13.4, TODO "GraphQL API" Phase 9): a query
// editor, a variables pane, a response pane, and the application's schema
// browsed from introspection.
//
// **Bundled with the admin SPA rather than vendored into every application.**
// The CDN GraphiQL every GraphQL server ships cannot load here — the admin is
// served under `default-src 'self'` — and putting GraphiQL into each *app*'s
// bundle would be a dependency the app never asked for, for a screen its users
// never see. What an admin actually needs from GraphiQL is these four panes, and
// the model behind them is `graphqlExplorer.ts`, which is where the parts that
// have to be right are tested.
//
// **It runs as the admin driving it, and says so.** Every operation goes through
// `runApplicationGraphql`, which hands the document to the application's own
// mounted provider with *this* admin's identity — same schema, same limits, same
// authorization at resolve time. An admin clears every role floor, so what this
// screen shows is the most any caller can see; the caption says that in as many
// words, because an explorer quietly holding more authority than the caller
// being debugged is a trap.

import { useCallback, useEffect, useMemo, useState, type KeyboardEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type { ListApplicationsResponse } from "../client";
import type { CurrentUser } from "../App";
import {
  INTROSPECTION_QUERY,
  buildRequest,
  graphqlEndpointUrl,
  graphqlMount,
  operationNames,
  parseSchema,
  responseView,
  runsAsCaption,
  starterDocument,
  type ResponseView,
  type SchemaOverview,
  type SchemaType,
} from "../graphqlExplorer";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader } from "../layout";
import { T, useT } from "../i18n";

type AppItem = ListApplicationsResponse[number];

export function GraphqlExplorer({ appId, user }: { appId: string; user: CurrentUser }) {
  const { t } = useT();
  const [app, setApp] = useState<AppItem | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [schema, setSchema] = useState<SchemaOverview | null>(null);
  // Kept apart from the run error: a schema that could not be fetched (the app
  // is not built yet, say) is a fact about the screen, not about the query the
  // admin is about to type.
  const [schemaError, setSchemaError] = useState<string | null>(null);
  const [document, setDocument] = useState("");
  const [variables, setVariables] = useState("");
  const [operation, setOperation] = useState("");
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [view, setView] = useState<ResponseView | null>(null);
  const [openType, setOpenType] = useState<string | null>(null);

  const mount = app ? graphqlMount(app.apis) : null;

  // Load the application, then ask it to describe itself. Introspection goes
  // through the very endpoint the queries do, so a screen that can browse the
  // schema is a screen whose Run button works.
  useEffect(() => {
    let cancelled = false;
    const run = async () => {
      let found: AppItem | undefined;
      try {
        found = (await api.listApplications()).find((a) => a.id === appId);
      } catch (err) {
        if (!cancelled) setLoadError(errorMessage(err, "Could not load the application."));
        return;
      }
      if (cancelled) return;
      if (!found) {
        setLoadError("There is no application with that id.");
        return;
      }
      setApp(found);
      if (!graphqlMount(found.apis)) return;
      try {
        const answer = await api.runApplicationGraphql(appId, { query: INTROSPECTION_QUERY });
        if (cancelled) return;
        const overview = parseSchema(answer);
        if (overview) {
          setSchema(overview);
          setOpenType(overview.queryType);
        } else {
          // A 200 that carried no schema is a refusal in the body; show it as
          // the response rather than as an empty pane.
          setSchemaError(
            responseView(answer)
              .errors.map((e) => e.message)
              .join("\n") || "The application returned no schema.",
          );
        }
      } catch (err) {
        if (!cancelled) {
          setSchemaError(
            errorMessage(err, "Could not read the application's schema. Is it built?"),
          );
        }
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [appId]);

  // Open on something runnable: the first root field, with its leaf columns.
  // Only while the editor is untouched — an admin who has typed owns it.
  useEffect(() => {
    if (!schema || document) return;
    const query = schema.types.find((t) => t.name === schema.queryType);
    const first = query?.fields[0];
    if (first) setDocument(starterDocument(schema, first.name));
  }, [schema, document]);

  const operations = useMemo(() => operationNames(document), [document]);

  const run = useCallback(async () => {
    const built = buildRequest(document, variables, operations.length > 1 ? operation : undefined);
    if (!built.ok) {
      setError(built.error);
      setView(null);
      return;
    }
    setRunning(true);
    setError(null);
    try {
      const answer = await api.runApplicationGraphql(appId, built.request);
      setView(responseView(answer));
    } catch (err) {
      // A GraphQL endpoint answers 200 with its errors in the body, so reaching
      // here means the *request* failed — the app is not mounted, the session
      // expired, the server is down.
      setError(errorMessage(err, "The request failed."));
      setView(null);
    } finally {
      setRunning(false);
    }
  }, [appId, document, operation, operations.length, variables]);

  // Ctrl/⌘-Enter runs, which is what every query console does and what an admin
  // iterating on a document will reach for before the button.
  const onEditorKey = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      if (!running) void run();
    }
  };

  const title = app ? `GraphQL — ${app.name}` : "GraphQL";

  return (
    <>
      <PageHeader
        pretitle={t("Applications")}
        title={title}
        actions={
          <>
            <Button variant="outline-secondary" href="#/applications">
              <IconArrowLeft className="icon-2" />
              <T text="Applications" />
            </Button>
            <Button onClick={() => void run()} disabled={running || !mount}>
              {running ? (
                <>
                  <Spinner animation="border" size="sm" role="status" /> <T text="Running…" />
                </>
              ) : (
                "Run (Ctrl+Enter)"
              )}
            </Button>
          </>
        }
      />
      <PageBody>
        {loadError && <Alert variant="danger">{loadError}</Alert>}

        {app && !mount && (
          <Alert variant="warning">
            <T
              text="{app} does not enable the GraphQL API provider, so there is nothing to explore."
              values={{ app: <strong>{app.name}</strong> }}
            />{" "}
            <T
              text="{link} and build the application."
              values={{
                link: (
                  <Alert.Link
                    href={`#/applications/${encodeURIComponent(app.id)}/edit`}
                  >
                    <T text="Add it to the application’s APIs" />
                  </Alert.Link>
                ),
              }}
            />
          </Alert>
        )}

        {app && mount && (
          <Alert variant="info">
            {runsAsCaption(user.email)}{" "}
            <span className="text-nowrap">
              <code>{graphqlEndpointUrl(app.subdomain, mount, window.location)}</code>
            </span>
          </Alert>
        )}

        {schemaError && <Alert variant="warning">{schemaError}</Alert>}

        <div className="row g-3">
          <div className="col-12 col-lg-4">
            <SchemaPane
              schema={schema}
              open={openType}
              onOpen={setOpenType}
              onUse={(field) => setDocument(starterDocument(schema, field))}
            />
          </div>

          <div className="col-12 col-lg-8">
            <div className="card mb-3">
              <div className="card-header">
                <h3 className="card-title"><T text="Query" /></h3>
              </div>
              <div className="card-body">
                <Form.Control
                  as="textarea"
                  rows={12}
                  className="font-monospace"
                  spellCheck={false}
                  value={document}
                  onChange={(e) => setDocument(e.target.value)}
                  onKeyDown={onEditorKey}
                  aria-label={t("GraphQL query")}
                />
                <Form.Label className="mt-3"><T text="Variables (JSON)" /></Form.Label>
                <Form.Control
                  as="textarea"
                  rows={4}
                  className="font-monospace"
                  spellCheck={false}
                  placeholder={t("{ \"floor\": 40000 }")}
                  value={variables}
                  onChange={(e) => setVariables(e.target.value)}
                  onKeyDown={onEditorKey}
                  aria-label={t("Variables")}
                />
                {/* Only when the document really declares several: GraphQL needs
                    an operation name exactly then, and a picker with one entry
                    is a control that does nothing. */}
                {operations.length > 1 && (
                  <>
                    <Form.Label className="mt-3"><T text="Operation" /></Form.Label>
                    <Form.Select
                      value={operation}
                      onChange={(e) => setOperation(e.target.value)}
                      aria-label={t("Operation")}
                    >
                      <option value=""><T text="(choose one)" /></option>
                      {operations.map((name) => (
                        <option key={name} value={name}>
                          {name}
                        </option>
                      ))}
                    </Form.Select>
                  </>
                )}
              </div>
            </div>

            {error && <Alert variant="danger">{error}</Alert>}
            <ResponsePane view={view} />
          </div>
        </div>
      </PageBody>
    </>
  );
}

/** The response, as its two halves — both of them when both are there. */
function ResponsePane({ view }: { view: ResponseView | null }) {
  const { t } = useT();
  if (!view) return null;
  return (
    <>
      {view.errors.length > 0 && (
        <Alert variant={view.partial ? "warning" : "danger"}>
          <div className="fw-bold mb-1">
            {view.partial
              ? "The server answered part of this query and refused the rest:"
              : "The server refused this query:"}
          </div>
          <ul className="mb-0">
            {view.errors.map((err, i) => (
              <li key={i}>
                {err.message}
                {err.path && (
                  <span className="text-secondary">
                    {t(" — at {path}", { path: err.path })}
                  </span>
                )}
                {err.code && <span className="text-secondary"> [{err.code}]</span>}
              </li>
            ))}
          </ul>
        </Alert>
      )}
      {view.data !== null && (
        <div className="card">
          <div className="card-header">
            <h3 className="card-title"><T text="Response" /></h3>
          </div>
          <div className="card-body">
            <pre className="mb-0 text-pre-wrap font-monospace">{view.data}</pre>
          </div>
        </div>
      )}
    </>
  );
}

/** The schema, browsed from introspection: one expandable row per type. */
function SchemaPane({
  schema,
  open,
  onOpen,
  onUse,
}: {
  schema: SchemaOverview | null;
  open: string | null;
  onOpen: (name: string | null) => void;
  onUse: (rootField: string) => void;
}) {
  if (!schema) {
    return (
      <div className="card">
        <div className="card-header">
          <h3 className="card-title"><T text="Schema" /></h3>
        </div>
        <div className="card-body text-secondary">
          <T text="The application's schema will appear here once it is built and serving." />
        </div>
      </div>
    );
  }
  // The roots first — they are what a query starts from — then everything else
  // alphabetically, which is how a schema of eighty tables stays navigable.
  const roots = [schema.queryType, schema.mutationType].filter((n): n is string => Boolean(n));
  const ordered = [
    ...roots.flatMap((name) => schema.types.filter((t) => t.name === name)),
    ...schema.types.filter((t) => !roots.includes(t.name)),
  ];
  return (
    <div className="card">
      <div className="card-header">
        <h3 className="card-title"><T text="Schema" /></h3>
      </div>
      <div className="list-group list-group-flush">
        {ordered.map((type) => (
          <TypeRow
            key={type.name}
            type={type}
            isQueryRoot={type.name === schema.queryType}
            open={open === type.name}
            onToggle={() => onOpen(open === type.name ? null : type.name)}
            onUse={onUse}
          />
        ))}
      </div>
    </div>
  );
}

function TypeRow({
  type,
  isQueryRoot,
  open,
  onToggle,
  onUse,
}: {
  type: SchemaType;
  isQueryRoot: boolean;
  open: boolean;
  onToggle: () => void;
  onUse: (rootField: string) => void;
}) {
  return (
    <div className="list-group-item">
      <button
        type="button"
        className="btn btn-link p-0 text-decoration-none text-start w-100"
        onClick={onToggle}
        aria-expanded={open}
      >
        <span className="fw-bold">{type.name}</span>{" "}
        <span className="text-secondary small">{type.kind.toLowerCase().replace("_", " ")}</span>
      </button>
      {open && (
        <ul className="list-unstyled mb-0 mt-2 small">
          {type.fields.map((field) => (
            <li key={field.name} className="mb-1">
              <code>{field.name}</code>
              {field.args.length > 0 && (
                <span className="text-secondary">
                  ({field.args.map((a) => `${a.name}: ${a.type}`).join(", ")})
                </span>
              )}
              {field.type && <span className="text-secondary">: {field.type}</span>}{" "}
              {/* Only on the query root: a field of `Departments` is not
                  something a document can start from, so a "use" button on one
                  would write a query that does not run. */}
              {isQueryRoot && (
                <button
                  type="button"
                  className="btn btn-link btn-sm p-0 align-baseline"
                  onClick={() => onUse(field.name)}
                >
                  <T text="use" />
                </button>
              )}
            </li>
          ))}
          {type.fields.length === 0 && <li className="text-secondary"><T text="No fields." /></li>}
        </ul>
      )}
    </div>
  );
}
