// A Saltcorn UI application's Views and Pages tabs (TODO "Saltcorn UI" 9.2):
// what the application serves, each row with its pattern, table and role and a
// link that opens it on the app's subdomain, and delete.
//
// There is no Build button anywhere near this: a Saltcorn UI application's
// source is these rows, and a delete is live on the app's next request.
// A rename and a delete say first what still refers to the view or page by its
// name, and which library items its layout places.
//
// A new view lands where v1's *Configure* does: in the builder when the first
// step its wizard stops at is a layout, else in the wizard. A page is built in
// the builder (**Edit**), and its properties are a form of their own
// (TODO "The builder" §9).

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { NO_BUILDER, builderPageUrl, openStep, viewLanding, type Landing } from "../builder";
import { useBuilderAvailable } from "../builderStatus";
import { IconArrowLeft } from "../icons";
import { AlertBody, PageBody, PageHeader, StatusBadge } from "../layout";
import { pageFormErrors, pageFormOf, savePageBody } from "../pageForm";
import { RoleSelect } from "../roleSelect";
import { useRoles } from "../roles";
import {
  NO_PAGES,
  NO_VIEWS,
  appTabs,
  createViewBody,
  deleteConfirmation,
  deleteReferenceLines,
  nameParam,
  newPageHref,
  newViewError,
  pageReferenceLines,
  pageReferencesReport,
  pagePropertiesHref,
  pageRows,
  referencesReport,
  saveViewBody,
  viewEditorHref,
  viewReferenceLines,
  viewRows,
  type AppItem,
  type AppTab,
  type Configuration,
  type NewViewForm,
  type PageItem,
  type PatternItem,
  type ViewItem,
} from "../views";
import { T, useT } from "../i18n";
import { activeTabId, settingsTabLabel } from "../settingsTabs";

/** The rename dialog: a view or a page, the name being typed, and what refers to
 * it — `null` while that is being found out. */
type Renaming = {
  kind: "view" | "page";
  original: string;
  name: string;
  report: string[] | null;
  error: string | null;
};

/** The tabs across the top of an application's screens: its framework's
 * settings tabs (TODO UX2.1). Links rather than state, because each tab is its
 * own route. Tabler's `.nav-pills`, as the Settings screen uses, with nothing
 * but classes doing the work under the admin's CSP. `active` is a tab's id or
 * the screen showing (`translations`), which highlights the tab holding it. */
export function ApplicationTabs({
  app,
  active,
}: {
  app: Pick<AppItem, "id" | "settings_tabs">;
  active: AppTab;
}) {
  const { t } = useT();
  const tabs = appTabs(app);
  if (tabs.length < 2) return null;
  const current = activeTabId(app.settings_tabs, active);
  return (
    <ul className="nav nav-pills mb-3">
      {tabs.map((tab) => (
        <li className="nav-item" key={tab.id}>
          <a
            className={`nav-link${tab.id === current ? " active" : ""}`}
            aria-current={tab.id === current ? "page" : undefined}
            href={tab.href}
          >
            {settingsTabLabel(tab.label, t)}
          </a>
        </li>
      ))}
    </ul>
  );
}

export function ApplicationViews({ appId, tab }: { appId: string; tab: "views" | "pages" }) {
  const { t } = useT();
  const roles = useRoles();
  const builderAvailable = useBuilderAvailable();
  const [app, setApp] = useState<AppItem | null>(null);
  const [views, setViews] = useState<ViewItem[] | null>(null);
  const [pages, setPages] = useState<PageItem[] | null>(null);
  const [patterns, setPatterns] = useState<PatternItem[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState<NewViewForm | null>(null);
  const [createError, setCreateError] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<Renaming | null>(null);
  const [busy, setBusy] = useState(false);

  const load = async () => {
    try {
      const found = (await api.listApplications()).find((a) => a.id === appId);
      if (!found) {
        setLoadError("That application no longer exists.");
        return;
      }
      setApp(found);
      const [v, p] = await Promise.all([api.listViews(appId), api.listPages(appId)]);
      setViews(v);
      setPages(p);
    } catch (err) {
      setLoadError(errorMessage(err, "Could not load the application's views and pages."));
    }
  };

  useEffect(() => {
    void load();
    // The patterns only annotate the list, so a server that cannot describe
    // them still lists the views.
    api
      .listViewPatterns()
      .then(setPatterns)
      .catch(() => setPatterns(null));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [appId]);

  /** What the delete warning says about references. A warning without them is
   * still a warning, so failing to find them does not stop the delete. */
  const deleteReferences = async (kind: "view" | "page", name: string): Promise<string[]> => {
    try {
      if (kind === "view") {
        const references = await api.viewReferences(appId, nameParam(name));
        return deleteReferenceLines(viewReferenceLines(references), references.places);
      }
      const references = await api.pageReferences(appId, nameParam(name));
      return deleteReferenceLines(pageReferenceLines(references), references.places);
    } catch {
      return [];
    }
  };

  const remove = async (kind: "view" | "page", name: string) => {
    if (!app) return;
    const references = await deleteReferences(kind, name);
    if (!window.confirm(deleteConfirmation(kind, name, app.name, references))) return;
    setError(null);
    try {
      if (kind === "view") {
        await api.deleteView(appId, nameParam(name));
      } else {
        await api.deletePage(appId, nameParam(name));
      }
      await load();
    } catch (err) {
      setError(errorMessage(err, `Could not delete the ${kind}.`));
    }
  };

  const create = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!creating) return;
    const problem = newViewError(creating, patterns);
    if (problem) {
      setCreateError(problem);
      return;
    }
    setBusy(true);
    setCreateError(null);
    try {
      const created = await api.createView(appId, createViewBody(creating));
      const context = (created.configuration ?? {}) as Configuration;
      let landing: Landing;
      try {
        const first = await openStep(
          (at) =>
            api.viewConfigStep(appId, {
              viewpattern: created.viewpattern,
              table_name: created.table_name ?? null,
              name: created.name,
              step: at,
              context,
            }),
          0,
          1,
        );
        landing = viewLanding(appId, created.name, first, builderAvailable === true);
      } catch {
        // The view exists; the wizard is where a step that cannot be built says why.
        landing = { kind: "wizard", route: viewEditorHref(appId, created.name).slice(1) };
      }
      setCreating(null);
      if (landing.kind === "builder") {
        window.location.assign(landing.url);
      } else {
        navigate(landing.route);
      }
    } catch (err) {
      setCreateError(errorMessage(err, "Could not create the view."));
    } finally {
      setBusy(false);
    }
  };

  /** Open the rename dialog, and find out what refers to the view or page
   * before any name is changed. */
  const startRename = async (kind: "view" | "page", original: string) => {
    setRenaming({ kind, original, name: original, report: null, error: null });
    const same = (r: Renaming | null) => r !== null && r.kind === kind && r.original === original;
    try {
      const report =
        kind === "view"
          ? referencesReport(original, await api.viewReferences(appId, nameParam(original)))
          : pageReferencesReport(original, await api.pageReferences(appId, nameParam(original)));
      setRenaming((r) => (r && same(r) ? { ...r, report } : r));
    } catch (err) {
      const message = errorMessage(err, `Could not find what refers to the ${kind}.`);
      setRenaming((r) => (r && same(r) ? { ...r, error: message } : r));
    }
  };

  const rename = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!renaming || !views || !pages) return;
    const { kind, original, name } = renaming;
    setBusy(true);
    try {
      if (kind === "view") {
        const view = views.find((v) => v.name === original);
        if (!view) throw new Error(`rename failed: 404: There is no view named "${original}".`);
        await api.saveView(
          appId,
          nameParam(original),
          saveViewBody(view, (view.configuration ?? {}) as Configuration, name.trim()),
        );
      } else {
        const page = pages.find((p) => p.name === original);
        if (!page) throw new Error(`rename failed: 404: There is no page named "${original}".`);
        const form = { ...pageFormOf(page), name };
        // A name another page has would be taken by `savePage` as that page.
        const taken = pageFormErrors(form, pages, original, null).name;
        if (taken) throw new Error(`rename failed: 409: ${taken}`);
        await api.savePage(appId, nameParam(original), savePageBody(form, page));
      }
      setRenaming(null);
      await load();
    } catch (err) {
      setRenaming({ ...renaming, error: errorMessage(err, `Could not rename the ${kind}.`) });
    } finally {
      setBusy(false);
    }
  };

  const header = (
    <PageHeader
      pretitle="Application"
      title={app?.name ?? "Application"}
      actions={
        <>
          {tab === "views" && app && (
            <Button
              onClick={() => {
                setCreateError(null);
                setCreating({
                  name: "",
                  description: "",
                  viewpattern: patterns?.[0]?.name ?? "List",
                  table_name: app.tables[0] ?? "",
                  min_role: 100,
                });
              }}
            >
              <T text="New view" />
            </Button>
          )}
          {tab === "pages" && app && (
            <a className="btn btn-primary" href={newPageHref(appId)}>
              <T text="New page" />
            </a>
          )}
          <Button variant="outline-secondary" onClick={() => navigate("/applications")}>
            <IconArrowLeft className="icon-2" />
            <T text="Applications" />
          </Button>
        </>
      }
    />
  );

  if (loadError) {
    return (
      <>
        {header}
        <PageBody>
          <Alert variant="danger">{loadError}</Alert>
        </PageBody>
      </>
    );
  }
  if (!app || !views || !pages) {
    return (
      <>
        {header}
        <PageBody>
          <Spinner animation="border" role="status" />
        </PageBody>
      </>
    );
  }

  const here = window.location;

  return (
    <>
      {header}
      <PageBody>
        <ApplicationTabs app={app} active={tab} />
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}

        {tab === "views" ? (
          <div className="card">
            <Table hover responsive className="card-table table-vcenter">
              <thead>
                <tr>
                  <th><T text="Name" /></th>
                  <th><T text="Pattern" /></th>
                  <th><T text="Table" /></th>
                  <th><T text="Role" /></th>
                  <th className="text-end"><T text="Actions" /></th>
                </tr>
              </thead>
              <tbody>
                {views.length === 0 && (
                  <tr>
                    <td colSpan={5} className="text-muted">
                      {NO_VIEWS}
                    </td>
                  </tr>
                )}
                {viewRows(views, patterns, roles, app.subdomain, here).map((row) => (
                  <tr key={row.name}>
                    <td>
                      <a href={row.url} target="_blank" rel="noreferrer">
                        {row.name}
                      </a>
                      {row.description && <div className="text-muted small">{row.description}</div>}
                    </td>
                    <td>
                      {row.pattern}
                      {row.patternMissing && (
                        <div>
                          <StatusBadge tone="red"><T text="not on this server" /></StatusBadge>
                        </div>
                      )}
                    </td>
                    <td>{row.table}</td>
                    <td>{row.role}</td>
                    <td className="text-end text-nowrap">
                      <a className="btn btn-sm btn-outline-primary me-1" href={viewEditorHref(appId, row.name)}>
                        <T text="Configure" />
                      </a>
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        className="me-1"
                        onClick={() => void startRename("view", row.name)}
                      >
                        <T text="Rename" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-danger"
                        onClick={() => void remove("view", row.name)}
                      >
                        <T text="Delete" />
                      </Button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </Table>
          </div>
        ) : (
          <div className="card">
            <Table hover responsive className="card-table table-vcenter">
              <thead>
                <tr>
                  <th><T text="Name" /></th>
                  <th><T text="Title" /></th>
                  <th><T text="Role" /></th>
                  <th><T text="Home page for" /></th>
                  <th className="text-end"><T text="Actions" /></th>
                </tr>
              </thead>
              <tbody>
                {pages.length === 0 && (
                  <tr>
                    <td colSpan={5} className="text-muted">
                      {NO_PAGES}
                    </td>
                  </tr>
                )}
                {pageRows(pages, roles, app.subdomain, here).map((row) => (
                  <tr key={row.name}>
                    <td>
                      <a href={row.url} target="_blank" rel="noreferrer">
                        {row.name}
                      </a>
                    </td>
                    <td>{row.title}</td>
                    <td>{row.role}</td>
                    <td>{row.homeFor.length ? row.homeFor.join(", ") : "—"}</td>
                    <td className="text-end text-nowrap">
                      {builderAvailable ? (
                        <a
                          className="btn btn-sm btn-outline-primary me-1"
                          href={builderPageUrl(appId, row.name)}
                        >
                          <T text="Edit" />
                        </a>
                      ) : (
                        // A disabled button swallows hover, so the reason sits on
                        // a wrapper.
                        <span className="d-inline-block me-1" title={builderAvailable === false ? NO_BUILDER : undefined}>
                          <Button size="sm" variant="outline-primary" disabled>
                            <T text="Edit" />
                          </Button>
                        </span>
                      )}
                      <a
                        className="btn btn-sm btn-outline-secondary me-1"
                        href={pagePropertiesHref(appId, row.name)}
                      >
                        <T text="Properties" />
                      </a>
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        className="me-1"
                        onClick={() => void startRename("page", row.name)}
                      >
                        <T text="Rename" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-danger"
                        onClick={() => void remove("page", row.name)}
                      >
                        <T text="Delete" />
                      </Button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </Table>
          </div>
        )}
        <Modal show={creating !== null} onHide={() => setCreating(null)}>
          <Form onSubmit={create}>
            <Modal.Header closeButton>
              <Modal.Title className="h4"><T text="New view" /></Modal.Title>
            </Modal.Header>
            <Modal.Body>
              {createError && <Alert variant="danger">{createError}</Alert>}
              {creating && (
                <>
                  <Form.Group className="mb-3" controlId="newViewName">
                    <Form.Label><T text="Name" /></Form.Label>
                    <Form.Control
                      value={creating.name}
                      autoFocus
                      required
                      onChange={(e) => setCreating({ ...creating, name: e.target.value })}
                    />
                    <Form.Text muted><T text="Also its address: /view/<name> on the app's subdomain." /></Form.Text>
                  </Form.Group>
                  <Form.Group className="mb-3" controlId="newViewPattern">
                    <Form.Label><T text="Pattern" /></Form.Label>
                    <Form.Select
                      value={creating.viewpattern}
                      onChange={(e) => setCreating({ ...creating, viewpattern: e.target.value })}
                    >
                      {(patterns ?? []).map((p) => (
                        <option key={p.name} value={p.name}>
                          {p.label || p.name}
                        </option>
                      ))}
                    </Form.Select>
                    {patterns?.find((p) => p.name === creating.viewpattern)?.description && (
                      <Form.Text muted>
                        {patterns.find((p) => p.name === creating.viewpattern)?.description}
                      </Form.Text>
                    )}
                  </Form.Group>
                  <Form.Group className="mb-3" controlId="newViewTable">
                    <Form.Label><T text="Table" /></Form.Label>
                    <Form.Select
                      value={creating.table_name}
                      onChange={(e) => setCreating({ ...creating, table_name: e.target.value })}
                    >
                      <option value="">—</option>
                      {app.tables.map((t) => (
                        <option key={t} value={t}>
                          {t}
                        </option>
                      ))}
                    </Form.Select>
                    <Form.Text muted><T text="One of the application's tables." /></Form.Text>
                  </Form.Group>
                  <RoleSelect
                    id="newViewRole"
                    label={t("Minimum role")}
                    roles={roles}
                    value={creating.min_role}
                    onChange={(min_role) => setCreating({ ...creating, min_role })}
                  />
                  <Form.Group className="mb-3" controlId="newViewDescription">
                    <Form.Label><T text="Description" /></Form.Label>
                    <Form.Control
                      value={creating.description}
                      onChange={(e) => setCreating({ ...creating, description: e.target.value })}
                    />
                  </Form.Group>
                </>
              )}
            </Modal.Body>
            <Modal.Footer>
              <Button variant="secondary" type="button" onClick={() => setCreating(null)}>
                <T text="Cancel" />
              </Button>
              <Button type="submit" disabled={busy || builderAvailable === null}>
                <T text="Create and configure" />
              </Button>
            </Modal.Footer>
          </Form>
        </Modal>

        <Modal show={renaming !== null} onHide={() => setRenaming(null)}>
          <Form onSubmit={rename}>
            <Modal.Header closeButton>
              <Modal.Title className="h4">
                {t("Rename {name}", { name: renaming?.original ?? "" })}
              </Modal.Title>
            </Modal.Header>
            <Modal.Body>
              {renaming?.error && <Alert variant="danger">{renaming.error}</Alert>}
              {renaming && (
                <>
                  <Form.Group className="mb-3" controlId="renameViewOrPage">
                    <Form.Label><T text="New name" /></Form.Label>
                    <Form.Control
                      value={renaming.name}
                      autoFocus
                      required
                      onChange={(e) => setRenaming({ ...renaming, name: e.target.value })}
                    />
                  </Form.Group>
                  {renaming.report === null ? (
                    !renaming.error && <Spinner animation="border" size="sm" role="status" />
                  ) : (
                    renaming.report.map((line) => (
                      <p key={line} className="mb-2">
                        {line}
                      </p>
                    ))
                  )}
                </>
              )}
            </Modal.Body>
            <Modal.Footer>
              <Button variant="secondary" type="button" onClick={() => setRenaming(null)}>
                <T text="Cancel" />
              </Button>
              <Button
                type="submit"
                disabled={
                  busy ||
                  !renaming ||
                  renaming.report === null ||
                  !renaming.name.trim() ||
                  renaming.name.trim() === renaming.original
                }
              >
                <T text="Rename" />
              </Button>
            </Modal.Footer>
          </Form>
        </Modal>
      </PageBody>
    </>
  );
}
