// MCP servers (TODO-agent-skills AS4.3): the remote Model Context Protocol
// servers an agent's `mcp_server` trait calls. Add one, test the connection,
// see its tools.
//
// A header's value is a credential, kept as an LLM provider's API key is: the
// server never sends it back (a saved one shows as ••••••••), and saving with
// the mask in place keeps what is stored. Testing works the same way, so a
// saved server is tested without retyping its key.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import { navigate } from "../App";
import { IconPlus, IconTrash } from "../icons";
import { PageBody, PageHeader } from "../layout";
import {
  bodyFromForm,
  emptyForm,
  formFromServer,
  nameProblem,
  type McpServerForm,
  type McpServerItem,
  type McpTestResult,
} from "../mcpServers";
import { T, useT } from "../i18n";

/** What a test or a tools listing found, shown under the list or the form. */
function TestOutcome({ result, onClose }: { result: McpTestResult; onClose: () => void }) {
  const { t } = useT();
  return (
    <Alert variant={result.ok ? "success" : "danger"} dismissible onClose={onClose}>
      <div className="text-break small">{result.message}</div>
      {result.server && (
        <div className="small text-muted mt-1">
          {result.server.name} {result.server.version} · {t("protocol")} {result.server.protocol_version}
        </div>
      )}
      {result.tools.length > 0 && (
        <Table size="sm" className="mt-2 mb-0">
          <tbody>
            {result.tools.map((tool) => (
              <tr key={tool.name}>
                <td className="font-monospace text-nowrap">
                  {tool.name}
                  {tool.read_only && (
                    <Badge bg="secondary-lt" className="ms-2">
                      <T text="read-only" />
                    </Badge>
                  )}
                </td>
                <td className="small">{tool.description}</td>
              </tr>
            ))}
          </tbody>
        </Table>
      )}
    </Alert>
  );
}

export function McpServers() {
  const { t } = useT();
  const [servers, setServers] = useState<McpServerItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<McpServerForm | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [testing, setTesting] = useState(false);
  const [formTest, setFormTest] = useState<McpTestResult | null>(null);
  const [listed, setListed] = useState<{ id: string; result: McpTestResult } | null>(null);
  const [listing, setListing] = useState<string | null>(null);

  const load = async () => {
    try {
      setServers(await api.listMcpServers());
    } catch (err) {
      setError(errorMessage(err, "Could not load MCP servers."));
    }
  };

  useEffect(() => {
    void load();
  }, []);

  const open = (form: McpServerForm) => {
    setEditing(form);
    setFormError(null);
    setFormTest(null);
  };

  const save = async (e: FormEvent) => {
    e.preventDefault();
    if (!editing) return;
    setSaving(true);
    setFormError(null);
    try {
      if (editing.id) {
        await api.updateMcpServer(editing.id, bodyFromForm(editing));
      } else {
        await api.createMcpServer(bodyFromForm(editing));
      }
      setEditing(null);
      await load();
    } catch (err) {
      setFormError(errorMessage(err, "Could not save the MCP server."));
    }
    setSaving(false);
  };

  const test = async () => {
    if (!editing) return;
    setTesting(true);
    setFormTest(null);
    setFormError(null);
    try {
      setFormTest(await api.testMcpServer({ id: editing.id, ...bodyFromForm(editing) }));
    } catch (err) {
      setFormError(errorMessage(err, "Could not test the MCP server."));
    }
    setTesting(false);
  };

  const showTools = async (server: McpServerItem, refresh: boolean) => {
    setListing(server.id);
    setError(null);
    try {
      setListed({ id: server.id, result: await api.listMcpServerTools(server.id, { refresh }) });
    } catch (err) {
      setError(errorMessage(err, `Could not list the tools of ${server.name}.`));
    }
    setListing(null);
  };

  const remove = async (server: McpServerItem) => {
    if (!window.confirm(t('Remove the MCP server "{name}"?', { name: server.name }))) return;
    setError(null);
    try {
      await api.deleteMcpServer(server.id);
      await load();
    } catch (err) {
      // The server names the agents still using it.
      setError(errorMessage(err, "Could not remove the MCP server."));
    }
  };

  const setHeader = (index: number, change: Partial<{ name: string; value: string }>) =>
    setEditing((cur) =>
      cur
        ? { ...cur, headers: cur.headers.map((h, i) => (i === index ? { ...h, ...change } : h)) }
        : cur,
    );

  const problem = editing ? nameProblem(editing.name) : null;

  return (
    <>
      <PageHeader
        pretitle="Agents"
        title={t("MCP servers")}
        actions={
          <>
            <Button variant="outline-secondary" onClick={() => navigate("/agents")}>
              <T text="Agents" />
            </Button>
            <Button onClick={() => open(emptyForm())}>
              <IconPlus className="icon-2" />
              <T text="New MCP server" />
            </Button>
          </>
        }
      />
      <PageBody>
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            {error}
          </Alert>
        )}
        <div className="card">
          <Table hover responsive className="card-table table-vcenter">
            <thead>
              <tr>
                <th><T text="Name" /></th>
                <th><T text="URL" /></th>
                <th><T text="Transport" /></th>
                <th className="text-end"><T text="Actions" /></th>
              </tr>
            </thead>
            <tbody>
              {servers?.length === 0 && (
                <tr>
                  <td colSpan={4} className="text-muted">
                    <T text="No MCP servers yet. Add one to give an agent its tools." />
                  </td>
                </tr>
              )}
              {servers?.map((server) => (
                <tr key={server.id}>
                  <td>
                    <span className="font-monospace">{server.name}</span>
                    {server.allow_private_network && (
                      <Badge bg="warning-lt" className="ms-2">
                        <T text="private network" />
                      </Badge>
                    )}
                    {server.description && <div className="text-muted small">{server.description}</div>}
                  </td>
                  <td className="text-break small">{server.url}</td>
                  <td className="small">{server.transport === "sse" ? "HTTP+SSE" : "Streamable HTTP"}</td>
                  <td className="text-end">
                    <div className="btn-list justify-content-end flex-nowrap">
                      <Button
                        size="sm"
                        variant="outline-secondary"
                        disabled={listing !== null}
                        onClick={() => void showTools(server, listed?.id === server.id)}
                      >
                        {listing === server.id ? (
                          "…"
                        ) : listed?.id === server.id ? (
                          <T text="Refresh tools" />
                        ) : (
                          <T text="Tools" />
                        )}
                      </Button>
                      <Button size="sm" variant="outline-secondary" onClick={() => open(formFromServer(server))}>
                        <T text="Edit" />
                      </Button>
                      <Button size="sm" variant="outline-danger" onClick={() => void remove(server)}>
                        <T text="Remove" />
                      </Button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        </div>
        {listed && (
          <div className="mt-3">
            <TestOutcome result={listed.result} onClose={() => setListed(null)} />
          </div>
        )}
        <p className="text-muted small mt-3">
          <T text="Header values, such as an Authorization token, are stored in the Saltcorn database like LLM API keys and are never sent back to this screen. This server makes the MCP calls itself, as the user chatting with the agent." />
        </p>
      </PageBody>

      <Modal show={editing !== null} onHide={() => setEditing(null)} size="lg">
        {editing && (
          <Form onSubmit={save}>
            <Modal.Header closeButton>
              <Modal.Title className="h4">
                {editing.id ? t("Edit MCP server") : t("New MCP server")}
              </Modal.Title>
            </Modal.Header>
            <Modal.Body>
              {formError && <Alert variant="danger">{formError}</Alert>}
              <Form.Group className="mb-3" controlId="mcpName">
                <Form.Label>
                  <T text="Name" /><span className="text-danger"> *</span>
                </Form.Label>
                <Form.Control
                  className="font-monospace"
                  value={editing.name}
                  required
                  isInvalid={editing.name !== "" && problem !== null}
                  onChange={(e) => setEditing({ ...editing, name: e.target.value })}
                />
                <Form.Control.Feedback type="invalid">{problem ? t(problem) : null}</Form.Control.Feedback>
                <Form.Text muted>
                  <T text="An agent offers this server's tools as name__tool." />
                </Form.Text>
              </Form.Group>
              <Form.Group className="mb-3" controlId="mcpDescription">
                <Form.Label><T text="Description" /></Form.Label>
                <Form.Control
                  value={editing.description}
                  onChange={(e) => setEditing({ ...editing, description: e.target.value })}
                />
              </Form.Group>
              <Form.Group className="mb-3" controlId="mcpUrl">
                <Form.Label>
                  <T text="URL" /><span className="text-danger"> *</span>
                </Form.Label>
                <Form.Control
                  className="font-monospace"
                  value={editing.url}
                  required
                  placeholder="https://mcp.example.com/mcp"
                  onChange={(e) => setEditing({ ...editing, url: e.target.value })}
                />
              </Form.Group>
              <Form.Group className="mb-3" controlId="mcpTransport">
                <Form.Label><T text="Transport" /></Form.Label>
                <Form.Select
                  value={editing.transport}
                  onChange={(e) =>
                    setEditing({ ...editing, transport: e.target.value as McpServerForm["transport"] })
                  }
                >
                  <option value="streamable_http">{t("Streamable HTTP (current servers)")}</option>
                  <option value="sse">{t("HTTP+SSE (older servers)")}</option>
                </Form.Select>
              </Form.Group>
              <Form.Label><T text="Headers" /></Form.Label>
              {editing.headers.map((header, index) => (
                <div className="d-flex gap-2 mb-2" key={index}>
                  <Form.Control
                    className="font-monospace"
                    style={{ maxWidth: "14rem" }}
                    placeholder={t("Header name")}
                    aria-label={t("Header name")}
                    value={header.name}
                    onChange={(e) => setHeader(index, { name: e.target.value })}
                  />
                  <Form.Control
                    className="font-monospace"
                    type="password"
                    autoComplete="off"
                    placeholder={t("Value")}
                    aria-label={t("Value")}
                    value={header.value}
                    onChange={(e) => setHeader(index, { value: e.target.value })}
                  />
                  <Button
                    variant="outline-secondary"
                    aria-label={t("Remove header")}
                    onClick={() =>
                      setEditing({ ...editing, headers: editing.headers.filter((_, i) => i !== index) })
                    }
                  >
                    <IconTrash className="icon" />
                  </Button>
                </div>
              ))}
              <Button
                size="sm"
                variant="outline-secondary"
                className="mb-3"
                onClick={() => setEditing({ ...editing, headers: [...editing.headers, { name: "", value: "" }] })}
              >
                <T text="Add header" />
              </Button>
              <Form.Group className="mb-3" controlId="mcpTimeout">
                <Form.Label><T text="Timeout (seconds)" /></Form.Label>
                <Form.Control
                  type="number"
                  min={0}
                  max={600}
                  placeholder="60"
                  value={editing.timeoutSecs}
                  onChange={(e) => setEditing({ ...editing, timeoutSecs: e.target.value })}
                />
              </Form.Group>
              <Form.Group className="mb-3" controlId="mcpPrivate">
                <Form.Check
                  type="checkbox"
                  label={t("Allow a private network address")}
                  checked={editing.allowPrivateNetwork}
                  onChange={(e) => setEditing({ ...editing, allowPrivateNetwork: e.target.checked })}
                />
                <Form.Text muted>
                  <T text="Off, a server on localhost or a private network (or one whose name resolves there) is refused, as for an agent's web fetches." />
                </Form.Text>
              </Form.Group>
              {formTest && <TestOutcome result={formTest} onClose={() => setFormTest(null)} />}
            </Modal.Body>
            <Modal.Footer>
              <Button
                variant="outline-secondary"
                className="me-auto"
                disabled={testing || problem !== null || editing.url.trim() === ""}
                onClick={() => void test()}
              >
                {testing ? t("Testing…") : t("Test connection")}
              </Button>
              <Button variant="outline-secondary" onClick={() => setEditing(null)}>
                <T text="Cancel" />
              </Button>
              <Button type="submit" disabled={saving || problem !== null}>
                {saving ? t("Saving…") : editing.id ? t("Save changes") : t("Add server")}
              </Button>
            </Modal.Footer>
          </Form>
        )}
      </Modal>
    </>
  );
}
