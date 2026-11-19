// A Saltcorn UI application's Library tab (TODO "The builder" §8, 9.4): its
// library items, what each is used by, rename, delete, and the layout as JSON.
//
// Items are made and edited in the builder (*Save as library component*, and
// editing a placed instance), so there is no New button and no layout editor
// here. The tab is only drawn for an application with views and pages
// (`appTabs`), and the server refuses the library of any other.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { useBuilderAvailable } from "../builderStatus";
import { IconArrowLeft } from "../icons";
import { applicationsCrumbs } from "../breadcrumbs";
import { AlertBody, PageBody, PageHeader } from "../layout";
import {
  NO_LIBRARY,
  isUsed,
  libraryDeleteConfirmation,
  libraryLayoutJson,
  renameLibraryBody,
  renameLibraryError,
  usedByLinks,
  usedBySummary,
  type LibraryItem,
} from "../library";
import type { AppItem } from "../views";
import { ApplicationTabs } from "./ApplicationViews";
import { T, useT } from "../i18n";

type Renaming = { item: LibraryItem; name: string; error: string | null };

export function ApplicationLibrary({ appId }: { appId: string }) {
  const { t } = useT();
  const builderAvailable = useBuilderAvailable();
  const [app, setApp] = useState<AppItem | null>(null);
  const [items, setItems] = useState<LibraryItem[] | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [renaming, setRenaming] = useState<Renaming | null>(null);
  const [showing, setShowing] = useState<LibraryItem | null>(null);
  const [busy, setBusy] = useState(false);

  const load = async () => {
    try {
      const found = (await api.listApplications()).find((a) => a.id === appId);
      if (!found) {
        setLoadError("That application no longer exists.");
        return;
      }
      setApp(found);
      if (!found.has_views) {
        setLoadError(`${found.name} has no library: only a Saltcorn UI application has one.`);
        return;
      }
      setItems(await api.listLibrary(appId));
    } catch (err) {
      setLoadError(errorMessage(err, "Could not load the application's library."));
    }
  };

  useEffect(() => {
    void load();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [appId]);

  const remove = async (item: LibraryItem) => {
    if (!app || !window.confirm(libraryDeleteConfirmation(item, app.name))) return;
    setError(null);
    try {
      await api.deleteLibraryItem(appId, item.id, { confirm: isUsed(item.used_by) });
      await load();
    } catch (err) {
      setError(errorMessage(err, "Could not delete the library item."));
    }
  };

  const rename = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!renaming || !items) return;
    const problem = renameLibraryError(items, renaming.item, renaming.name);
    if (problem) {
      setRenaming({ ...renaming, error: problem });
      return;
    }
    setBusy(true);
    try {
      await api.saveLibraryItem(appId, renaming.item.id, renameLibraryBody(renaming.item, renaming.name));
      setRenaming(null);
      await load();
    } catch (err) {
      setRenaming({ ...renaming, error: errorMessage(err, "Could not rename the library item.") });
    } finally {
      setBusy(false);
    }
  };

  const header = (
    <PageHeader
      crumbs={applicationsCrumbs(t)}
      title={app?.name ?? "Application"}
      actions={
        <Button variant="outline-secondary" onClick={() => navigate("/applications")}>
          <IconArrowLeft className="icon-2" />
          <T text="Applications" />
        </Button>
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
  if (!app || !items) {
    return (
      <>
        {header}
        <PageBody>
          <Spinner animation="border" role="status" />
        </PageBody>
      </>
    );
  }

  return (
    <>
      {header}
      <PageBody>
        <ApplicationTabs app={app} active="library" />
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}
        <div className="card">
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Icon" /></th>
                <th><T text="Name" /></th>
                <th><T text="Used by" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {items.length === 0 && (
                <tr>
                  <td colSpan={4} className="text-muted">
                    {NO_LIBRARY}
                  </td>
                </tr>
              )}
              {items.map((item) => {
                const open = expanded === item.id;
                return (
                  <tr key={item.id}>
                    {/* v1's icon is a Font Awesome class, which the admin UI does
                        not load, so it is shown as the class it is. */}
                    <td>{item.icon ? <code>{item.icon}</code> : <span className="text-muted">—</span>}</td>
                    <td>
                      {item.name}
                      {item.description && <div className="text-muted small">{item.description}</div>}
                    </td>
                    <td>
                      {isUsed(item.used_by) ? (
                        <>
                          <Button
                            variant="link"
                            className="p-0"
                            aria-expanded={open}
                            onClick={() => setExpanded(open ? null : item.id)}
                          >
                            {usedBySummary(item.used_by)}
                          </Button>
                          {open && (
                            <ul className="list-unstyled small mb-0 mt-1">
                              {usedByLinks(appId, item.used_by, builderAvailable === true).map((use) => (
                                <li key={`${use.kind}:${use.name}`}>
                                  <span className="text-muted">{use.kind}</span>{" "}
                                  {use.href ? <a href={use.href}>{use.name}</a> : use.name}
                                </li>
                              ))}
                            </ul>
                          )}
                        </>
                      ) : (
                        <span className="text-muted">{usedBySummary(item.used_by)}</span>
                      )}
                    </td>
                    <td className="text-end text-nowrap">
                      <Button
                        size="sm"
                        variant="outline-primary"
                        className="me-1"
                        onClick={() => setShowing(item)}
                      >
                        <T text="Layout" />
                      </Button>
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        className="me-1"
                        onClick={() => setRenaming({ item, name: item.name, error: null })}
                      >
                        <T text="Rename" />
                      </Button>
                      <Button size="sm" variant="outline-danger" onClick={() => void remove(item)}>
                        <T text="Delete" />
                      </Button>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </Table>
        </div>

        <Modal show={showing !== null} onHide={() => setShowing(null)} size="lg">
          <Modal.Header closeButton>
            <Modal.Title className="h4">{showing?.name}</Modal.Title>
          </Modal.Header>
          <Modal.Body>
            <p className="text-muted">
              <T text="The layout as saved. It is edited in the builder, inside any view or page that places it." />
            </p>
            {showing && <pre className="small mb-0">{libraryLayoutJson(showing)}</pre>}
          </Modal.Body>
        </Modal>

        <Modal show={renaming !== null} onHide={() => setRenaming(null)}>
          <Form onSubmit={rename}>
            <Modal.Header closeButton>
              <Modal.Title className="h4">
                {t("Rename {name}", { name: renaming?.item.name ?? "" })}
              </Modal.Title>
            </Modal.Header>
            <Modal.Body>
              {renaming?.error && <Alert variant="danger">{renaming.error}</Alert>}
              {renaming && (
                <Form.Group controlId="renameLibraryItem">
                  <Form.Label><T text="New name" /></Form.Label>
                  <Form.Control
                    value={renaming.name}
                    autoFocus
                    required
                    onChange={(e) => setRenaming({ ...renaming, name: e.target.value })}
                  />
                  <Form.Text muted>
                    <T text="Views and pages place an item by its id, so they keep finding it under the new name." />
                  </Form.Text>
                </Form.Group>
              )}
            </Modal.Body>
            <Modal.Footer>
              <Button variant="secondary" type="button" onClick={() => setRenaming(null)}>
                <T text="Cancel" />
              </Button>
              <Button
                type="submit"
                disabled={busy || !renaming || renaming.name.trim() === renaming.item.name}
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
