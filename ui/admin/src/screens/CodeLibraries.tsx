// Code library screen (TODO F3.3): Settings → Development → Code library.
//
// Named, shared JavaScript that code queries and triggers import with
// `lib(name)` — instance-wide, or one application's own. The list says where
// each is visible and who imports it ("used by" is a scan of the queries' and
// triggers' code for `lib("name")`, so it is a warning before an edit rather
// than an authority). The editor is the one code queries use, with the same
// typings plus `exports` and `module`. A save is compile-checked by the server
// (without running anything), and a syntax error comes back naming its line.

import { useEffect, useMemo, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Col from "react-bootstrap/Col";
import Form from "react-bootstrap/Form";
import Row from "react-bootstrap/Row";
import Table from "react-bootstrap/Table";

import { api } from "../api";
import type { ListApplicationsResponse } from "../client";
import { CodeEditor } from "../CodeEditor";
import {
  blankDraft,
  draftOf,
  importLine,
  libraryBody,
  scopeLabel,
  sortLibraries,
  useLabel,
  validLibraryName,
  type CodeLibrary,
  type LibraryDraft,
} from "../codeLibraries";
import { forgetLibraryNames } from "../codeTypes";
import { PageBody, PageHeader } from "../layout";
import { T, useT } from "../i18n";

export function CodeLibraries() {
  const { t } = useT();
  const [libraries, setLibraries] = useState<CodeLibrary[] | null>(null);
  const [apps, setApps] = useState<ListApplicationsResponse>([]);
  const [draft, setDraft] = useState<LibraryDraft>(blankDraft());
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const load = async (): Promise<CodeLibrary[]> => {
    try {
      const listed = sortLibraries(await api.listCodeLibraries());
      setLibraries(listed);
      return listed;
    } catch {
      setError("Could not load the code libraries.");
      return [];
    }
  };

  useEffect(() => {
    void load();
    void api
      .listApplications()
      .then(setApps)
      .catch(() => setApps([]));
  }, []);

  const selected = useMemo(
    () => libraries?.find((l) => l.id === draft.id) ?? null,
    [libraries, draft.id],
  );

  const edit = (change: Partial<LibraryDraft>) => {
    setSaved(null);
    setDraft((d) => ({ ...d, ...change }));
  };

  const save = async () => {
    setBusy(true);
    setError(null);
    setSaved(null);
    try {
      const body = libraryBody(draft);
      const answer = draft.id
        ? await api.updateCodeLibrary(draft.id, body)
        : await api.createCodeLibrary(body);
      forgetLibraryNames();
      await load();
      setDraft(draftOf(answer));
      setSaved(answer.name);
    } catch (e) {
      // The server's message is the point: "SyntaxError: Unexpected token ';'
      // (line 4)" is what the admin needs to fix the code.
      setError(e instanceof Error ? e.message : "Could not save the library.");
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!draft.id) return;
    const users = selected?.used_by?.length ?? 0;
    if (
      users > 0 &&
      !window.confirm(
        t("{count} queries or triggers import this library and will fail. Delete it anyway?", {
          count: users,
        }),
      )
    ) {
      return;
    }
    setBusy(true);
    setError(null);
    try {
      await api.deleteCodeLibrary(draft.id);
      forgetLibraryNames();
      setDraft(blankDraft());
      setSaved(null);
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not delete the library.");
    } finally {
      setBusy(false);
    }
  };

  const nameOk = validLibraryName(draft.name.trim());

  return (
    <>
      <PageHeader pretitle="Settings · Development" title={t("Code library")} />
      <PageBody>
        <p className="text-muted">
          <T text="Shared JavaScript for code queries and triggers. Import a library with lib(name); an application's own library wins over an instance-wide one of the same name, and triggers see instance-wide libraries only. A library runs with the importing code's own db, fs, fetch and user." />
        </p>
        {error && (
          <Alert variant="danger" className="code-library-error" style={{ whiteSpace: "pre-wrap" }}>
            {error}
          </Alert>
        )}
        {saved && (
          <Alert variant="success">
            <T text="Saved {name}. Code that imports it uses the new version on its next run." args={{ name: saved }} />
          </Alert>
        )}
        <Row>
          <Col lg={4} className="mb-4">
            <div className="card">
              <div className="card-header d-flex justify-content-between align-items-center">
                <h3 className="card-title"><T text="Libraries" /></h3>
                <Button
                  size="sm"
                  variant="outline-primary"
                  onClick={() => {
                    setDraft(blankDraft());
                    setSaved(null);
                    setError(null);
                  }}
                >
                  <T text="New library" />
                </Button>
              </div>
              <Table hover responsive className="card-table table-vcenter code-library-list">
                <thead>
                  <tr>
                    <th><T text="Name" /></th>
                    <th><T text="Scope" /></th>
                    <th><T text="Used by" /></th>
                  </tr>
                </thead>
                <tbody>
                  {libraries?.length === 0 && (
                    <tr>
                      <td colSpan={3} className="text-muted"><T text="No libraries yet." /></td>
                    </tr>
                  )}
                  {libraries?.map((l) => (
                    <tr
                      key={l.id}
                      data-library={l.name}
                      className={l.id === draft.id ? "table-active" : undefined}
                      style={{ cursor: "pointer" }}
                      onClick={() => {
                        setDraft(draftOf(l));
                        setSaved(null);
                        setError(null);
                      }}
                    >
                      <td>
                        <code>{l.name}</code>
                        {l.description && <div className="text-muted small">{l.description}</div>}
                      </td>
                      <td className="small">{scopeLabel(l)}</td>
                      <td className="small">{l.used_by?.length ?? 0}</td>
                    </tr>
                  ))}
                </tbody>
              </Table>
            </div>
          </Col>
          <Col lg={8} className="mb-4">
            <Card>
              <Card.Header>
                {draft.id ? <T text="Edit library" /> : <T text="New library" />}
              </Card.Header>
              <Card.Body>
                <Row>
                  <Col md={4}>
                    <Form.Group className="mb-2" controlId="libraryName">
                      <Form.Label><T text="Name" /></Form.Label>
                      <Form.Control
                        value={draft.name}
                        isInvalid={draft.name !== "" && !nameOk}
                        onChange={(e) => edit({ name: e.target.value })}
                      />
                      <Form.Text muted><code>{importLine(draft.name.trim())}</code></Form.Text>
                    </Form.Group>
                  </Col>
                  <Col md={4}>
                    <Form.Group className="mb-2" controlId="libraryScope">
                      <Form.Label><T text="Scope" /></Form.Label>
                      <Form.Select
                        value={draft.applicationId}
                        onChange={(e) => edit({ applicationId: e.target.value })}
                      >
                        <option value="">{t("Instance-wide")}</option>
                        {apps.map((a) => (
                          <option key={a.id} value={a.id}>
                            {t("App: {name}", { name: a.subdomain })}
                          </option>
                        ))}
                      </Form.Select>
                    </Form.Group>
                  </Col>
                  <Col md={4}>
                    <Form.Group className="mb-2" controlId="libraryDescription">
                      <Form.Label><T text="Description" /></Form.Label>
                      <Form.Control
                        value={draft.description}
                        onChange={(e) => edit({ description: e.target.value })}
                      />
                    </Form.Group>
                  </Col>
                </Row>
                <div className="mb-3">
                  <CodeEditor
                    key={draft.id ?? "new"}
                    id="libraryCode"
                    language="javascript"
                    scope={{ library: true }}
                    value={draft.code}
                    onChange={(code) => edit({ code })}
                  />
                </div>
                {selected?.used_by && selected.used_by.length > 0 && (
                  <div className="mb-3 small code-library-used-by">
                    <strong><T text="Used by" /></strong>:{" "}
                    {selected.used_by.map((u) => useLabel(u)).join(", ")}
                  </div>
                )}
                <div className="d-flex gap-2">
                  <Button onClick={() => void save()} disabled={busy || !nameOk}>
                    <T text="Save" />
                  </Button>
                  {draft.id && (
                    <Button variant="outline-danger" onClick={() => void remove()} disabled={busy}>
                      <T text="Delete" />
                    </Button>
                  )}
                </div>
              </Card.Body>
            </Card>
          </Col>
        </Row>
      </PageBody>
    </>
  );
}

/** The link in Settings → Development. */
export function CodeLibraryPanel() {
  return (
    <Card className="mt-4">
      <Card.Header><T text="Code library" /></Card.Header>
      <Card.Body>
        <p className="text-muted">
          <T text="Shared JavaScript that code queries and triggers import with lib(name)." />
        </p>
        <a className="btn btn-outline-primary" href="#/code-libraries">
          <T text="Open the code library" />
        </a>
      </Card.Body>
    </Card>
  );
}
