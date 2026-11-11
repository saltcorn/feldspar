// A Saltcorn UI page's properties (TODO "The builder" §7, 9.3): **New page**,
// and **Properties** on an existing one.
//
// v1's page properties form, saved through `savePage`. **Create** opens the new
// page in the builder, which is v1's redirect to `/pageedit/edit/:name`; saving
// an existing page returns to the Pages tab. Changing an existing page's name is
// a rename, and says first what still refers to the page by its old name, as the
// rename on the Pages tab does. The builder's **Page properties** link lands here.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { builderPageUrl } from "../builder";
import { useBuilderAvailable } from "../builderStatus";
import { IconArrowLeft } from "../icons";
import { AlertBody, PageBody, PageHeader } from "../layout";
import { newPageForm, pageFormErrors, pageFormOf, savePageBody, type PageForm } from "../pageForm";
import { RoleSelect } from "../roleSelect";
import { useRoles } from "../roles";
import {
  nameParam,
  pageReferencesReport,
  type AppItem,
  type PageItem,
  type PageReferences,
} from "../views";
import { ApplicationTabs } from "./ApplicationViews";
import { T, useT } from "../i18n";

export function PageProperties({ appId, name }: { appId: string; name: string | null }) {
  const { t } = useT();
  const roles = useRoles();
  const builderAvailable = useBuilderAvailable();
  const [app, setApp] = useState<AppItem | null>(null);
  const [pages, setPages] = useState<PageItem[] | null>(null);
  const [page, setPage] = useState<PageItem | null>(null);
  const [form, setForm] = useState<PageForm | null>(null);
  const [references, setReferences] = useState<PageReferences | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [submitted, setSubmitted] = useState(false);
  const [busy, setBusy] = useState(false);

  const pagesRoute = `/applications/${encodeURIComponent(appId)}/pages`;

  useEffect(() => {
    void (async () => {
      try {
        const found = (await api.listApplications()).find((a) => a.id === appId);
        if (!found) {
          setLoadError("That application no longer exists.");
          return;
        }
        setApp(found);
        const listed = await api.listPages(appId);
        setPages(listed);
        if (name === null) {
          setForm(newPageForm());
          return;
        }
        const existing = listed.find((p) => p.name === name);
        if (!existing) {
          setLoadError(`${found.name} has no page named "${name}".`);
          return;
        }
        setPage(existing);
        setForm(pageFormOf(existing));
        // Only a rename needs them, and the form works without.
        api
          .pageReferences(appId, nameParam(existing.name))
          .then(setReferences)
          .catch(() => setReferences(null));
      } catch (err) {
        setLoadError(errorMessage(err, "Could not load the application's pages."));
      }
    })();
  }, [appId, name]);

  const errors = form && pages ? pageFormErrors(form, pages, page?.name ?? null, roles) : {};
  const renamed = page !== null && form !== null && form.name.trim() !== page.name;

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!form) return;
    setSubmitted(true);
    if (Object.keys(errors).length > 0) return;
    setBusy(true);
    setError(null);
    try {
      const body = savePageBody(form, page);
      const saved = await api.savePage(appId, nameParam(page ? page.name : body.name), body);
      if (!page && builderAvailable) {
        window.location.assign(builderPageUrl(appId, saved.name));
      } else {
        navigate(pagesRoute);
      }
    } catch (err) {
      setError(errorMessage(err, "Could not save the page."));
      setBusy(false);
    }
  };

  const header = (
    <PageHeader
      pretitle={app ? `${app.name} · page` : "Page"}
      title={name ?? "New page"}
      docTitle={name == null ? t("New page") : t("{name} page", { name })}
      actions={
        <>
          {page && builderAvailable && (
            <a className="btn btn-outline-primary" href={builderPageUrl(appId, page.name)}>
              <T text="Open in builder" />
            </a>
          )}
          <Button variant="outline-secondary" onClick={() => navigate(pagesRoute)}>
            <IconArrowLeft className="icon-2" />
            <T text="Pages" />
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
  if (!app || !form || !pages) {
    return (
      <>
        {header}
        <PageBody>
          <Spinner animation="border" role="status" />
        </PageBody>
      </>
    );
  }

  const set = (patch: Partial<PageForm>) => setForm({ ...form, ...patch });

  return (
    <>
      {header}
      <PageBody>
        <ApplicationTabs app={app} active="pages" />
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}
        <Form className="card" onSubmit={submit} noValidate>
          <div className="card-header">
            <h3 className="card-title">{page ? "Page properties" : "New page"}</h3>
          </div>
          <div className="card-body">
            <Form.Group className="mb-3" controlId="pageName">
              <Form.Label><T text="Name" /></Form.Label>
              <Form.Control
                value={form.name}
                autoFocus={!page}
                isInvalid={submitted && Boolean(errors.name)}
                onChange={(e) => set({ name: e.target.value })}
              />
              <Form.Control.Feedback type="invalid">{errors.name}</Form.Control.Feedback>
              <Form.Text muted><T text="Also its address: /page/<name> on the app's subdomain." /></Form.Text>
            </Form.Group>
            {renamed && (
              <Alert variant="warning">
                {references === null ? (
                  <Spinner animation="border" size="sm" role="status" />
                ) : (
                  pageReferencesReport(page.name, references).map((line) => (
                    <p key={line} className="mb-1">
                      {line}
                    </p>
                  ))
                )}
              </Alert>
            )}
            <Form.Group className="mb-3" controlId="pageTitle">
              <Form.Label><T text="Title" /></Form.Label>
              <Form.Control value={form.title} onChange={(e) => set({ title: e.target.value })} />
              <Form.Text muted><T text="The browser tab's title while the page is open." /></Form.Text>
            </Form.Group>
            <Form.Group className="mb-3" controlId="pageDescription">
              <Form.Label><T text="Description" /></Form.Label>
              <Form.Control
                value={form.description}
                onChange={(e) => set({ description: e.target.value })}
              />
            </Form.Group>
            <RoleSelect
              id="pageRole"
              label={t("Minimum role")}
              roles={roles}
              value={form.min_role}
              onChange={(min_role) => set({ min_role })}
            >
              <T text="The least privileged role that can open the page." />
            </RoleSelect>
            {submitted && errors.min_role && (
              <div className="text-danger small mb-3">{errors.min_role}</div>
            )}
            <Form.Check
              className="mb-2"
              id="pageNoMenu"
              label={t("No menu: show the page without the application's menu")}
              checked={form.no_menu}
              onChange={(e) => set({ no_menu: e.target.checked })}
            />
            <Form.Check
              id="pageFluid"
              label={t("Fluid layout: use the full width of the window")}
              checked={form.request_fluid_layout}
              onChange={(e) => set({ request_fluid_layout: e.target.checked })}
            />
          </div>
          <div className="card-footer d-flex gap-2">
            <Button variant="outline-secondary" type="button" onClick={() => navigate(pagesRoute)}>
              <T text="Cancel" />
            </Button>
            <Button className="ms-auto" type="submit" disabled={busy || builderAvailable === null}>
              {page ? (renamed ? "Rename and save" : "Save") : builderAvailable ? "Create and build" : "Create"}
            </Button>
          </div>
        </Form>
      </PageBody>
    </>
  );
}
