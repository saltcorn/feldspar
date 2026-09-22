// Create / edit an application. The point this screen proves: **no screen knows
// a specific framework's settings**. The admin picks a framework and the form
// renders whatever settings that framework's `config_spec` declares (design
// §13.2/§13.3) — there is no `code`-framework-specific code here. `ui/form-runtime`
// is out of MVP scope, so the spec is rendered with a plain form.
//
// The rest of the form covers the other pieces of an application record: its
// subdomain, its table and file-store subsets, its enabled APIs, its static
// directories, and its CSP.
//
// A framework whose applications have views and pages (Saltcorn UI) has a dozen
// settings that are the running application's — the menu, the login form, the
// languages, the home page per role. Those are not on this form: a new
// application takes their defaults, and an existing one edits them on its own
// App settings tab, which is this same screen showing only them.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type {
  CreateApplicationRequest,
  ListApiProvidersResponse,
  ListApplicationsResponse,
  ListFileStoresResponse,
  ListFrameworksResponse,
  ListStreamsResponse,
  ListTablesResponse,
  ListTriggersResponse,
} from "../client";
import { navigate } from "../App";
import { IconArrowLeft } from "../icons";
import { PageBody, PageHeader } from "../layout";
import { setNotice } from "../notice";
import { SettingsFields, asString, buildConfig, readConfig } from "../settings";
import {
  apiRowsFromApp,
  apiRowsToRequest,
  blankApiRow,
  specFor,
  supportsCustomQueries,
  type ApiRow,
} from "../apiRows";
import { MultiSelect } from "../multiSelect";
import {
  blankStaticRow,
  staticDirsToRequest,
  storeOptions,
  type StaticRow,
} from "../staticDirs";
import { CustomQueries } from "./CustomQueries";
import { ApplicationTabs } from "./ApplicationViews";
import { appTabs, settingsOnOwnTab } from "../views";
import { T, useT } from "../i18n";

type FrameworkInfo = ListFrameworksResponse[number];
type AppItem = ListApplicationsResponse[number];
type TriggerItem = ListTriggersResponse[number];
type StreamItem = ListStreamsResponse[number];
type ApiProviderInfo = ListApiProvidersResponse[number];
type TableItem = ListTablesResponse[number];
type FileStoreItem = ListFileStoresResponse[number];
/** A `{ mount, store, path }` static-directory row. */

/** Render a CSP object as `directive: src1 src2` lines for the textarea. */
function cspToText(csp: unknown): string {
  if (!csp || typeof csp !== "object") return "";
  return Object.entries(csp as Record<string, unknown>)
    .map(([name, sources]) => {
      const list = Array.isArray(sources) ? sources.map(asString) : [];
      return `${name}: ${list.join(" ")}`.trimEnd();
    })
    .join("\n");
}

/** Parse `directive: src1 src2` lines back into a CSP object. */
function textToCsp(text: string): Record<string, string[]> {
  const csp: Record<string, string[]> = {};
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    const colon = trimmed.indexOf(":");
    const name = (colon === -1 ? trimmed : trimmed.slice(0, colon)).trim();
    const rest = colon === -1 ? "" : trimmed.slice(colon + 1);
    if (name) csp[name] = rest.split(/\s+/).filter((s) => s.length > 0);
  }
  return csp;
}

export function ApplicationForm({
  appId,
  tab = "settings",
}: {
  appId?: string;
  /** Which of the application's tabs this is: the record, or the framework's
   * own settings when they have a tab of their own. Both save the whole
   * application, so each tab keeps what the other one holds. */
  tab?: "settings" | "app-settings";
}) {
  const { t } = useT();
  const [frameworks, setFrameworks] = useState<FrameworkInfo[] | null>(null);
  // The server's triggers, so the exposed subset is *picked* rather than typed:
  // a name that does not resolve is an application that will not mount, and the
  // list is right here to choose from. The tables and the file stores are here
  // for the same reason, and were the last two subsets an admin had to type from
  // memory as a comma-separated list.
  const [allTriggers, setAllTriggers] = useState<TriggerItem[]>([]);
  // …and the server's streams, for the same reason and with the same rule: a
  // stream is reachable from outside only because an app named it.
  const [allStreams, setAllStreams] = useState<StreamItem[]>([]);
  const [allTables, setAllTables] = useState<TableItem[]>([]);
  const [allFileStores, setAllFileStores] = useState<FileStoreItem[]>([]);
  // The API providers this server registers, for the same reason: a provider name
  // is the one field of an application whose typo survives the save and turns up
  // later as "unknown API provider" from a mount that failed.
  const [allProviders, setAllProviders] = useState<ApiProviderInfo[]>([]);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // The stored application being edited, for the tabs above the form: an
  // application whose source is views and pages has two more screens.
  const [stored, setStored] = useState<AppItem | null>(null);

  const [name, setName] = useState("");
  const [description, setDescription] = useState("");
  const [subdomain, setSubdomain] = useState("");
  const [frameworkName, setFrameworkName] = useState("");
  const [config, setConfig] = useState<Record<string, string>>({});
  const [tables, setTables] = useState<string[]>([]);
  const [fileStores, setFileStores] = useState<string[]>([]);
  const [triggers, setTriggers] = useState<string[]>([]);
  const [streams, setStreams] = useState<string[]>([]);
  // A new application starts with REST at `/api`. An app with no API has no
  // endpoints, which for a React app means a generated client with no methods and
  // a project that cannot compile — and for any app means a UI that cannot reach
  // its data. It is a row like any other, so removing it stays one click.
  const [apis, setApis] = useState<ApiRow[]>([
    { ...blankApiRow(), provider: "rest", mount: "/api" },
  ]);
  const [staticDirs, setStaticDirs] = useState<StaticRow[]>([]);
  // Empty by default: a new app takes its framework's policy (§2.2) unless the
  // admin states one. Editing an app fills this in from what was stored.
  const [csp, setCsp] = useState("");

  // Load the frameworks (always) and, when editing, the app to prefill.
  useEffect(() => {
    let cancelled = false;
    const run = async () => {
      try {
        const fws = await api.listFrameworks();
        // A server with no triggers, or one that cannot list them, still edits
        // applications: the picker is simply empty, and an app that already
        // names a trigger keeps naming it.
        const trigs = await api.listTriggers().catch(() => [] as TriggerItem[]);
        // A server built without stream support answers this with an error, and
        // the picker is simply empty: an app that already names a stream keeps
        // naming it.
        const strms = await api.listStreams().catch(() => [] as StreamItem[]);
        // Likewise: a server that cannot list its providers still edits
        // applications, with the provider box falling back to free text.
        const provs = await api
          .listApiProviders()
          .catch(() => [] as ApiProviderInfo[]);
        // And likewise for the two subsets: a listing that fails leaves an empty
        // picker holding whatever the application already names, rather than a
        // screen that will not open.
        const tbls = await api.listTables().catch(() => [] as TableItem[]);
        const stores = await api
          .listFileStores()
          .catch(() => [] as FileStoreItem[]);
        let existing: AppItem | undefined;
        if (appId) {
          existing = (await api.listApplications()).find((a) => a.id === appId);
          if (!existing) {
            if (!cancelled) setLoadError("That application no longer exists.");
            return;
          }
        }
        if (cancelled) return;
        setFrameworks(fws);
        setAllTriggers(trigs);
        setAllStreams(strms);
        setAllProviders(provs);
        setAllTables(tbls);
        setAllFileStores(stores);
        if (existing) {
          setStored(existing);
          setName(existing.name);
          setDescription(existing.description);
          setSubdomain(existing.subdomain);
          setFrameworkName(existing.framework.name);
          setConfig(readConfig(existing.framework.config));
          setTables(existing.tables);
          setFileStores(existing.file_stores);
          setTriggers(existing.triggers);
          setStreams(existing.streams);
          setApis(apiRowsFromApp(existing.apis));
          setStaticDirs(
            existing.static_dirs.map((d) => ({
              mount: d.mount,
              store: d.store,
              path: d.path,
            })),
          );
          setCsp(cspToText(existing.csp));
        } else {
          setFrameworkName(fws[0]?.name ?? "");
        }
      } catch {
        if (!cancelled) setLoadError("Could not load the framework list.");
      }
    };
    void run();
    return () => {
      cancelled = true;
    };
  }, [appId]);

  const selected = frameworks?.find((f) => f.name === frameworkName);
  const ownTab = settingsOnOwnTab(selected);
  const appSettingsHref = stored
    ? appTabs(stored).find((t) => t.id === "app-settings")?.href
    : undefined;

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      const body: CreateApplicationRequest = {
        name: name.trim(),
        description: description.trim(),
        subdomain: subdomain.trim(),
        framework: {
          name: frameworkName,
          config: buildConfig(selected?.config_spec ?? [], config),
        },
        extra_frameworks: [],
        tables,
        file_stores: fileStores,
        triggers,
        streams,
        apis: apiRowsToRequest(apis, allProviders),
        static_dirs: staticDirsToRequest(staticDirs),
        // An empty box means "no opinion", and is sent as no field at all so the
        // *framework's* default policy applies (§2.2) — a React app gets the one
        // fitted to what Vite emits. Sending `default-src 'self'` because a
        // textarea was pre-filled would silently overrule that.
        csp: csp.trim() ? textToCsp(csp) : undefined,
        attributes: {},
      };
      if (appId) {
        await api.updateApplication(appId, body);
      } else {
        const created = await api.createApplication(body);
        // Creating a React application also creates its project on the server
        // (§2.3), and creating any application creates the agent that builds it
        // (§13.3). Both are news the admin should see, and so is either one being
        // refused — on an application that was still created, because the row is
        // valid either way. The list screen owns the banner, so the message is
        // handed to it rather than shown here on a screen about to unmount.
        const done: string[] = [];
        const refused: string[] = [];
        if (created.scaffolded) done.push(created.scaffolded);
        // The server starts the first build itself, so the application serves on
        // its subdomain without anybody pressing anything — the Build button is
        // for the build after that, and for one that failed.
        if (created.building) {
          done.push(
            "Its first build has started; it serves on its subdomain when that finishes.",
          );
        }
        if (created.scaffold_error) refused.push(created.scaffold_error);
        if (created.agent) {
          done.push(
            `Its builder agent, ${created.agent}, is ready to chat with.`,
          );
        }
        if (created.agent_error) refused.push(created.agent_error);
        if (done.length || refused.length) {
          setNotice({
            ok: refused.length === 0,
            title: refused.length
              ? `Application created, but not everything with it — ${created.name}`
              : `Application created — ${created.name}`,
            text: [...refused, ...done].join(" "),
          });
        }
      }
      navigate("/applications");
    } catch (err) {
      setError(errorMessage(err, "Could not save the application."));
    } finally {
      setBusy(false);
    }
  };

  if (loadError) {
    return (
      <PageBody>
        <Alert variant="danger">{loadError}</Alert>
      </PageBody>
    );
  }
  if (!frameworks) {
    return (
      <PageBody>
        <div className="text-center py-5">
          <Spinner animation="border" role="status" />
        </div>
      </PageBody>
    );
  }

  return (
    <>
      <PageHeader
        pretitle="Deploy"
        title={appId ? "Edit application" : "New application"}
        actions={
          <Button
            variant="outline-secondary"
            onClick={() => navigate("/applications")}
          >
            <IconArrowLeft className="icon-2" />
            <T text="Back" />
          </Button>
        }
      />
      <PageBody>
        {stored && <ApplicationTabs app={stored} active={tab} />}
        {error && <Alert variant="danger">{error}</Alert>}

        {tab === "app-settings" ? (
          <Form onSubmit={submit}>
            {ownTab ? (
              <Card className="mb-3">
                <Card.Header>
                  {t("{framework} settings", {
                    framework: selected?.label || frameworkName,
                  })}
                </Card.Header>
                <Card.Body>
                  <SettingsFields
                    spec={selected?.config_spec ?? []}
                    values={config}
                    onChange={(name, v) =>
                      setConfig((c) => ({ ...c, [name]: v }))
                    }
                  />
                </Card.Body>
              </Card>
            ) : (
              // Reached by typing the address, or after switching the
              // application to a framework whose settings are on its form.
              <Alert variant="info">
                <T text="This application's framework settings are on its Settings tab." />
              </Alert>
            )}
            {ownTab && (
              <Button type="submit" disabled={busy}>
                {busy ? "Saving…" : "Save changes"}
              </Button>
            )}
          </Form>
        ) : (
          <Form onSubmit={submit}>
            <Row>
              <Col md={6}>
                <Form.Group className="mb-3" controlId="appName">
                  <Form.Label><T text="Name" /></Form.Label>
                  <Form.Control
                    value={name}
                    required
                    onChange={(e) => setName(e.target.value)}
                  />
                </Form.Group>
              </Col>
              <Col md={6}>
                <Form.Group className="mb-3" controlId="appSubdomain">
                  <Form.Label><T text="Subdomain" /></Form.Label>
                  <Form.Control
                    value={subdomain}
                    required
                    onChange={(e) => setSubdomain(e.target.value)}
                  />
                  <Form.Text muted>
                    {t("Served at {subdomain}.your-domain.", {
                      subdomain: subdomain || "<subdomain>",
                    })}
                  </Form.Text>
                </Form.Group>
              </Col>
            </Row>

            <Form.Group className="mb-3" controlId="appDescription">
              <Form.Label><T text="Description" /></Form.Label>
              <Form.Control
                value={description}
                onChange={(e) => setDescription(e.target.value)}
              />
            </Form.Group>

            <Card className="mb-3">
              <Card.Header><T text="Framework" /></Card.Header>
              <Card.Body>
                {/* One choice per framework, each with the name and sentence the
                  *server* supplied. Two frameworks are not two equal names in a
                  dropdown — one creates the project for you and the other hands you
                  the paths — and that difference has to reach the admin. It does so
                  as data: the label, the description and the order all come from
                  the registry (§2.2/§2.4), so this screen presents the distinction
                  without knowing which framework is which. The first offered is the
                  one an admin should take, and is what a new application starts on. */}
                <fieldset className="mb-3">
                  <legend className="form-label"><T text="Framework" /></legend>
                  {frameworks.map((f) => (
                    <Form.Check
                      key={f.name}
                      type="radio"
                      name="framework"
                      id={`framework-${f.name}`}
                      className="mb-2"
                      checked={f.name === frameworkName}
                      onChange={() => setFrameworkName(f.name)}
                      label={
                        <>
                          <span className="fw-semibold">
                            {f.label || f.name}
                          </span>
                          {f.description && (
                            <div className="text-muted small">
                              {f.description}
                            </div>
                          )}
                        </>
                      }
                    />
                  ))}
                </fieldset>

                {/* The framework's own settings, rendered from its config_spec — no
                  framework-specific code lives here. Picking the first framework
                  shows its two settings and picking the other shows five, with no
                  branch in this file: the spec is the branch. */}
                {ownTab ? (
                  <Form.Text muted>
                    {/* One sentence with a hole in it, not three fragments: a
                      translator has to be able to move the link. */}
                    {appId ? (
                      <T
                        text="Its menu, login form, languages and other settings are on the application’s {tab} tab."
                        values={{
                          tab: appSettingsHref ? (
                            <a href={appSettingsHref}>{t("App settings")}</a>
                          ) : (
                            t("App settings")
                          ),
                        }}
                      />
                    ) : (
                      <T
                        text="Its menu, login form, languages and other settings are on the application’s {tab} tab, once it is created."
                        values={{ tab: t("App settings") }}
                      />
                    )}
                  </Form.Text>
                ) : (
                  <SettingsFields
                    spec={selected?.config_spec ?? []}
                    values={config}
                    onChange={(name, v) =>
                      setConfig((c) => ({ ...c, [name]: v }))
                    }
                  />
                )}
              </Card.Body>
            </Card>

            <Row>
              <Col md={6}>
                <Form.Group className="mb-3">
                  <Form.Label htmlFor="appTables"><T text="Tables" /></Form.Label>
                  {/* The catalog, ticked — not a comma-separated list typed from
                    memory. A table's label is worth showing beside its name for
                    the same reason the tables screen shows it: the name is what
                    the app addresses and the label is what it is. */}
                  <MultiSelect
                    id="appTables"
                    options={allTables.map((table) => ({
                      value: table.name,
                      description:
                        table.label && table.label !== table.name
                          ? table.label
                          : table.description,
                    }))}
                    selected={tables}
                    onChange={setTables}
                    emptyText="This server has no tables yet."
                  />
                  <Form.Text muted><T text="The tables this app may access." /></Form.Text>
                </Form.Group>
              </Col>
              <Col md={6}>
                <Form.Group className="mb-3">
                  <Form.Label htmlFor="appFileStores"><T text="File stores" /></Form.Label>
                  <MultiSelect
                    id="appFileStores"
                    options={allFileStores.map((s) => ({
                      value: s.name,
                      // A store that is configured but unreachable is still a
                      // legitimate choice — the app names it, and the connection is
                      // the store's own problem to fix — so it is offered with the
                      // fact attached rather than withheld.
                      description: s.connected
                        ? s.description
                        : [s.description, "not connected"]
                            .filter(Boolean)
                            .join(" — "),
                    }))}
                    selected={fileStores}
                    onChange={setFileStores}
                    emptyText="This server has no file stores yet."
                  />
                  <Form.Text muted>
                    <T text="The file stores this app may access." />
                  </Form.Text>
                </Form.Group>
              </Col>
            </Row>

            <Card className="mb-3">
              <Card.Header><T text="Triggers" /></Card.Header>
              <Card.Body>
                {allTriggers.length === 0 && (
                  <div className="text-muted">
                    <T text="No triggers are configured on this server." />
                  </div>
                )}
                {allTriggers.map((trigger) => (
                  <Form.Check
                    key={trigger.id}
                    type="checkbox"
                    id={`trigger-${trigger.id}`}
                    className="mb-2"
                    checked={triggers.includes(trigger.name)}
                    onChange={(e) =>
                      setTriggers((current) =>
                        e.target.checked
                          ? [...current, trigger.name]
                          : current.filter((n) => n !== trigger.name),
                      )
                    }
                    label={
                      <>
                        <span className="fw-semibold">{trigger.name}</span>
                        <div className="text-muted small">
                          {/* A workflow body has no action to name (§10.3). */}
                          {t("{body} · on {event} · {access}", {
                            body: trigger.action ?? "workflow",
                            event: trigger.when,
                            // Same vocabulary the trigger form uses: 1 is admin,
                            // 100 is public, and no role set means admins only.
                            access:
                              trigger.min_role == null
                                ? t("admins only (no minimum role set)")
                                : t("minimum role {role}", {
                                    role: trigger.min_role,
                                  }),
                          })}
                        </div>
                      </>
                    }
                  />
                ))}
                {/* An app that names a trigger the server no longer has will not
                  mount, so a stale selection is shown rather than dropped on the
                  floor by a picker that only knows about triggers that exist. */}
                {triggers
                  .filter((name) => !allTriggers.some((t) => t.name === name))
                  .map((name) => (
                    <div key={name} className="text-danger small mb-2">
                      <span className="fw-semibold">{name}</span> <T text="— no trigger of that name exists here, so this application will not mount until it is removed or the trigger is recreated." />
                      <Button
                        size="sm"
                        variant="outline-danger"
                        className="ms-2"
                        onClick={() =>
                          setTriggers((current) =>
                            current.filter((n) => n !== name),
                          )
                        }
                      >
                        <T text="Remove" />
                      </Button>
                    </div>
                  ))}
                <Form.Text muted>
                  <T
                    text="Each ticked trigger is exposed as {route} on this app, guarded by the trigger’s own minimum role."
                    values={{
                      route: <code>POST {"{api mount}"}/actions/{"{name}"}</code>,
                    }}
                  />
                </Form.Text>
              </Card.Body>
            </Card>

            <Card className="mb-3">
              <Card.Header><T text="Streams" /></Card.Header>
              <Card.Body>
                {allStreams.length === 0 && (
                  <div className="text-muted">
                    <T text="No streams are configured on this server." />
                  </div>
                )}
                {allStreams.map((s) => (
                  <Form.Check
                    key={s.id}
                    type="checkbox"
                    id={`stream-${s.id}`}
                    className="mb-2"
                    checked={streams.includes(s.name)}
                    onChange={(e) =>
                      setStreams((current) =>
                        e.target.checked
                          ? [...current, s.name]
                          : current.filter((n) => n !== s.name),
                      )
                    }
                    label={
                      <>
                        <span className="fw-semibold">{s.name}</span>
                        <div className="text-muted small">
                          {s.provider}
                          {s.enabled ? "" : " · disabled"} ·{" "}
                          {/* The stream's own floor, in the vocabulary the rest
                            of the admin UI uses: no role set means admins. */}
                          {s.min_role == null
                            ? "admins only (no minimum role set)"
                            : `minimum role ${s.min_role}`}
                        </div>
                      </>
                    }
                  />
                ))}
                {/* A named stream that is gone is shown rather than dropped, for
                  the reason a stale trigger is: the app keeps naming it until
                  somebody decides otherwise. */}
                {streams
                  .filter((name) => !allStreams.some((s) => s.name === name))
                  .map((name) => (
                    <div key={name} className="text-danger small mb-2">
                      <span className="fw-semibold">{name}</span> <T text="— no stream of that name exists here, so this application cannot observe it until it is removed or the stream is recreated." />
                      <Button
                        size="sm"
                        variant="outline-danger"
                        className="ms-2"
                        onClick={() =>
                          setStreams((current) =>
                            current.filter((n) => n !== name),
                          )
                        }
                      >
                        <T text="Remove" />
                      </Button>
                    </div>
                  ))}
                <Form.Text muted>
                  <T
                    text="Each ticked stream can be observed at {route} over a WebSocket, guarded by the stream’s own minimum role, and appears in this app’s generated client as {call}."
                    values={{
                      route: (
                        <code>
                          {"{api mount}"}/streams/{"{name}"}/observe
                        </code>
                      ),
                      call: <code>observeStream_{"{name}"}()</code>,
                    }}
                  />
                </Form.Text>
              </Card.Body>
            </Card>

            <ApiRows
              rows={apis}
              providers={allProviders}
              tables={tables}
              onChange={setApis}
            />

            <RepeatableRows
              title={t("Static directories")}
              rows={staticDirs}
              columns={[
                { key: "mount", label: t("Mount"), placeholder: "/docs" },
                {
                  key: "store",
                  label: t("Store"),
                  // The stores *this application* declares, from the picker
                  // twenty lines above rather than from the server's full list:
                  // the server refuses a static directory outside the subset, so
                  // anything else here would be a choice that cannot be saved.
                  options: (value) =>
                    storeOptions(fileStores, value).map((o) => ({
                      value: o.value,
                      label: o.declared
                        ? o.value
                        : t("{name} (not a declared store)", { name: o.value }),
                    })),
                  emptyText: t(
                    "Tick a file store above to serve a directory from it.",
                  ),
                },
                { key: "path", label: t("Path"), placeholder: "handbook" },
              ]}
              onChange={setStaticDirs}
              blank={blankStaticRow()}
            />
            <Form.Text muted className="d-block mb-3">
              <T text="A directory is served at the app's own sub-path — `/docs/guide.png` for the store's `handbook/guide.png`. A file still has to be readable by whoever asks: a mount says where files appear, not that they are public." />
            </Form.Text>

            <Form.Group className="mb-3" controlId="appCsp">
              <Form.Label>Content-Security-Policy</Form.Label>
              <Form.Control
                as="textarea"
                rows={3}
                value={csp}
                onChange={(e) => setCsp(e.target.value)}
              />
              <Form.Text muted>
                <T text="One directive per line, e.g. `default-src: 'self'`. Leave empty to use the framework's own default policy." />
              </Form.Text>
            </Form.Group>

            <Button type="submit" disabled={busy}>
              {busy ? "Saving…" : appId ? "Save changes" : "Create application"}
            </Button>
          </Form>
        )}
      </PageBody>
    </>
  );
}

/** The APIs list: one card per enabled provider — its name, its mount, and the
 * settings *that provider declares*, rendered by the same `SettingsFields` a
 * framework's are (§13.3).
 *
 * Its own control rather than a `RepeatableRows` row, because an API row is no
 * longer uniform strings: two providers on one application show different
 * settings, and the difference comes from the server. There is still no
 * provider-specific code here — enabling GraphQL shows its aggregation switch
 * and its four bounds because that is what its `config_spec` says. */
function ApiRows({
  rows,
  providers,
  tables,
  onChange,
}: {
  rows: ApiRow[];
  providers: ApiProviderInfo[];
  /** The application's declared tables, sent with a custom query's check so the
   * server can refuse a name or path this app's own table routes already hold. */
  tables: string[];
  onChange: (rows: ApiRow[]) => void;
}) {
  const { t } = useT();
  const setRow = (index: number, next: ApiRow) =>
    onChange(rows.map((r, i) => (i === index ? next : r)));
  return (
    <Card className="mb-3">
      <Card.Header className="d-flex justify-content-between align-items-center">
        <span><T text="APIs" /></span>
        <Button
          size="sm"
          variant="outline-primary"
          onClick={() => onChange([...rows, blankApiRow()])}
        >
          <T text="Add" />
        </Button>
      </Card.Header>
      <Card.Body>
        {rows.length === 0 && <div className="text-muted">None.</div>}
        {rows.map((row, index) => {
          const spec = specFor(providers, row.provider);
          return (
            <div
              key={index}
              className={index > 0 ? "border-top pt-3 mt-3" : undefined}
            >
              <Row className="mb-2 align-items-end">
                <Col>
                  <Form.Label className="small mb-1"><T text="Provider" /></Form.Label>
                  {/* The registered names, from the server. An empty list — a
                      server that could not list them — leaves this the text box
                      it was: a picker that cannot be populated should not become
                      a field that cannot be filled in. */}
                  {providers.length > 0 ? (
                    <Form.Select
                      value={row.provider}
                      onChange={(e) => {
                        const provider = e.target.value;
                        setRow(index, {
                          ...row,
                          provider,
                          // Picking a provider fills an *empty* mount with that
                          // provider's usual sub-path, so the common case is one
                          // click. An admin who has typed a mount keeps it.
                          mount: row.mount.trim()
                            ? row.mount
                            : (providers.find((p) => p.name === provider)
                                ?.default_mount ?? ""),
                        });
                      }}
                    >
                      <option value=""><T text="Choose…" /></option>
                      {providers.map((p) => (
                        <option key={p.name} value={p.name}>
                          {p.label}
                        </option>
                      ))}
                      {/* A stored value this server does not register — a
                          provider from a plugin that is gone, or an older name.
                          Kept as an option so opening the form does not silently
                          change it. */}
                      {row.provider &&
                        !providers.some((p) => p.name === row.provider) && (
                          <option value={row.provider}>
                            {t("{name} (not registered)", {
                              name: row.provider,
                            })}
                          </option>
                        )}
                    </Form.Select>
                  ) : (
                    <Form.Control
                      value={row.provider}
                      placeholder={t("rest")}
                      onChange={(e) =>
                        setRow(index, { ...row, provider: e.target.value })
                      }
                    />
                  )}
                </Col>
                <Col>
                  <Form.Label className="small mb-1"><T text="Mount" /></Form.Label>
                  <Form.Control
                    value={row.mount}
                    placeholder="/api"
                    onChange={(e) =>
                      setRow(index, { ...row, mount: e.target.value })
                    }
                  />
                </Col>
                <Col xs="auto">
                  <Button
                    variant="outline-danger"
                    onClick={() => onChange(rows.filter((_, i) => i !== index))}
                  >
                    <T text="Remove" />
                  </Button>
                </Col>
              </Row>
              {spec.length > 0 && (
                <div className="ps-1">
                  <SettingsFields
                    spec={spec}
                    values={row.config}
                    idPrefix={`api-${index}`}
                    onChange={(name, v) =>
                      setRow(index, {
                        ...row,
                        config: { ...row.config, [name]: v },
                      })
                    }
                  />
                </div>
              )}
              {/* …and its custom SQL queries, when the provider serves them.
                  Offered on the provider's own say-so (`supports_custom_queries`),
                  so this screen still knows nothing about which provider REST is. */}
              {supportsCustomQueries(providers, row.provider) && (
                <CustomQueries
                  queries={row.queries}
                  tables={tables}
                  idPrefix={`api-${index}`}
                  onChange={(queries) => setRow(index, { ...row, queries })}
                />
              )}
            </div>
          );
        })}
        {providers.length > 0 && (
          <Form.Text muted>
            {providers.map((p) => (
              <div key={p.name}>
                <code>{p.name}</code> — {p.description}
              </div>
            ))}
            <div className="mt-1">
              <T text="Each provider is mounted on its own sub-path; two on the same one is refused, because a request resolves to only one of them." />
            </div>
          </Form.Text>
        )}
      </Card.Body>
    </Card>
  );
}

/** One column of a {@link RepeatableRows} list. A plain text box, unless it
 * declares `options` — then a drop-down over them, computed from the row's
 * current value so a stored value the list no longer offers can be kept and
 * shown rather than silently dropped. */
type RepeatableColumn<T> = {
  key: keyof T & string;
  label: string;
  placeholder?: string;
  /** The drop-down's options for a cell currently holding `value`. */
  options?: (value: string) => { value: string; label: string }[];
  /** Shown under a drop-down that offers nothing, saying where to fix it. */
  emptyText?: string;
};

/** A repeatable list of uniform string-field rows (static dirs), with
 * add/remove. Generic over the row shape. */
function RepeatableRows<T extends Record<string, string>>({
  title,
  rows,
  columns,
  blank,
  onChange,
}: {
  title: string;
  rows: T[];
  columns: RepeatableColumn<T>[];
  blank: T;
  onChange: (rows: T[]) => void;
}) {
  const { t } = useT();
  const setCell = (index: number, key: keyof T & string, value: string) => {
    onChange(
      rows.map((r, i) => (i === index ? ({ ...r, [key]: value } as T) : r)),
    );
  };
  return (
    <Card className="mb-3">
      <Card.Header className="d-flex justify-content-between align-items-center">
        <span>{title}</span>
        <Button
          size="sm"
          variant="outline-primary"
          onClick={() => onChange([...rows, { ...blank }])}
        >
          <T text="Add" />
        </Button>
      </Card.Header>
      <Card.Body>
        {rows.length === 0 && <div className="text-muted">None.</div>}
        {rows.map((row, index) => (
          <Row key={index} className="mb-2 align-items-end">
            {columns.map((col) => {
              // A column with `options` is a drop-down: the set is short, known
              // and already on the screen, so typing one of its members into a
              // box is asking a question whose answer is visible.
              const options = col.options?.(row[col.key]);
              return (
                <Col key={col.key}>
                  <Form.Label className="small mb-1">{col.label}</Form.Label>
                  {options ? (
                    <>
                      <Form.Select
                        value={row[col.key]}
                        onChange={(e) => setCell(index, col.key, e.target.value)}
                      >
                        <option value="">{t("Choose…")}</option>
                        {options.map((o) => (
                          <option key={o.value} value={o.value}>
                            {o.label}
                          </option>
                        ))}
                      </Form.Select>
                      {options.length === 0 && col.emptyText && (
                        <Form.Text muted>{col.emptyText}</Form.Text>
                      )}
                    </>
                  ) : (
                    <Form.Control
                      value={row[col.key]}
                      placeholder={col.placeholder}
                      onChange={(e) => setCell(index, col.key, e.target.value)}
                    />
                  )}
                </Col>
              );
            })}
            <Col xs="auto">
              <Button
                variant="outline-danger"
                onClick={() => onChange(rows.filter((_, i) => i !== index))}
              >
                <T text="Remove" />
              </Button>
            </Col>
          </Row>
        ))}
      </Card.Body>
    </Card>
  );
}
