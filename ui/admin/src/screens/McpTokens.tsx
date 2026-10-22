// The Development tab's second panel: the API tokens an external coding agent
// administers this installation with (§13.6).
//
// It sits beside the Python reading because it is the same kind of thing — a
// tab's worth of setting, and then the thing the setting is *for*. What is
// deliberate about it:
//
// - **Nothing is offered while the server is off.** A token minted against an
//   unticked checkbox authenticates against a route that answers 404, and an
//   admin who finds that out has spent an afternoon on it. The switch is
//   directly above; saying so is cheaper than a support call.
// - **The plaintext is shown once, in a box that says so**, with the
//   `claude mcp add` line already built around it. The credential exists in the
//   mint response and nowhere else — the table holds a hash — so the moment it
//   leaves this screen it is gone.
// - **The two sentences at the bottom are load-bearing.** A token is an
//   administrator, and revoking it is the only way to take it back. An admin who
//   reads nothing else on this panel should read those.
// - **Revoke, not delete.** The row stays, marked, because a revocation is a
//   thing that happened and this list is where it is seen to have happened.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Form from "react-bootstrap/Form";
import Spinner from "react-bootstrap/Spinner";

import { api } from "../api";
import { AlertBody } from "../layout";
import {
  type ApiToken,
  type MintForm,
  claudeMcpAddLine,
  emptyMint,
  grantFields,
  grantedKeys,
  holdsAll,
  mcpUrl,
  mintProblem,
  mintRequest,
  shortGrantLabel,
  tokenBadge,
  when,
} from "../mcpTokens";
import type { FieldSpec } from "../settings";
import { T, useT } from "../i18n";

export function McpTokensPanel({ enabled }: { enabled: boolean }) {
  const { t } = useT();
  const [fields, setFields] = useState<FieldSpec[] | null>(null);
  const [tokens, setTokens] = useState<ApiToken[] | null>(null);
  const [form, setForm] = useState<MintForm | null>(null);
  const [secret, setSecret] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    void (async () => {
      try {
        // The checkboxes are the copilot's own (§13.6): one vocabulary for what
        // an agent may do, whichever kind of agent it is.
        const spec = grantFields(await api.listAgentTraits());
        setFields(spec);
        setForm(emptyMint(spec));
        setTokens(await api.listApiTokens());
      } catch (e) {
        setError(e instanceof Error ? e.message : "Could not load the API tokens.");
      }
    })();
  }, []);

  const problem = form ? mintProblem(form) : null;

  const mint = async (e: FormEvent) => {
    e.preventDefault();
    if (!form || !fields || problem) return;
    setBusy(true);
    setError(null);
    try {
      const minted = await api.createApiToken(mintRequest(form, fields));
      setSecret(minted.secret);
      setCopied(false);
      setForm(emptyMint(fields));
      setTokens(await api.listApiTokens());
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not generate the token.");
    } finally {
      setBusy(false);
    }
  };

  const revoke = async (token: ApiToken) => {
    setError(null);
    try {
      await api.revokeApiToken(token.id);
      setTokens(await api.listApiTokens());
    } catch (e) {
      setError(e instanceof Error ? e.message : "Could not revoke the token.");
    }
  };

  const origin = typeof window === "undefined" ? "" : window.location.origin;
  const command = claudeMcpAddLine(origin, secret);

  const copy = async () => {
    try {
      await navigator.clipboard.writeText(command);
      setCopied(true);
    } catch {
      // A clipboard the browser will not hand over is not worth a banner: the
      // line is on screen and selectable, which is the fallback.
      setCopied(false);
    }
  };

  return (
    <div className="card mb-4">
      <div className="card-header">
        <div>
          <h3 className="card-title"><T text="Administration MCP tokens" /></h3>
          <p className="card-subtitle text-secondary mb-0">
            <T text="Bearer credentials for the MCP server above, so a coding agent can read and change this installation’s schema, triggers and applications." />{" "}
            <T
              text="Each one runs with {whose} authority, bounded by the boxes ticked when it is generated."
              values={{
                whose: (
                  <strong>
                    <T text="your" />
                  </strong>
                ),
              }}
            />
          </p>
        </div>
      </div>
      <div className="card-body">
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}

        {!enabled && (
          <Alert variant="warning">
            <AlertBody>
              <T
                text="The administration MCP server is turned off, so {url} answers 404 and no token is looked at."
                values={{ url: <code>{mcpUrl(origin)}</code> }}
              />{" "}
              <T
                text="Tick {setting} above and save before generating one."
                values={{
                  setting: (
                    <strong>
                      <T text="Administration MCP server" />
                    </strong>
                  ),
                }}
              />
            </AlertBody>
          </Alert>
        )}

        {secret && (
          <Alert variant="success">
            <AlertBody>
              <p className="mb-2">
                <strong><T text="This is the only time this token is shown." /></strong> <T text="It is stored as a hash, so nothing — not this screen, not the log, not the database — can read it back. Copy the line below now; if you lose it, revoke the token and generate another." />
              </p>
              <pre className="border rounded p-3 mb-2 text-wrap">{command}</pre>
              <div className="btn-list">
                <Button variant="outline-secondary" size="sm" onClick={() => void copy()}>
                  {copied ? "Copied" : "Copy the command"}
                </Button>
                <Button variant="outline-secondary" size="sm" onClick={() => setSecret(null)}>
                  <T text="Done" />
                </Button>
              </div>
            </AlertBody>
          </Alert>
        )}

        {enabled && !secret && (
          <div className="mb-3">
            <p className="text-secondary mb-2">
              <T text="Register this server with a coding agent by running this, with a token of its own in place of the placeholder — a generated token is shown in full exactly once, and this line is shown with it." />
            </p>
            <pre className="border rounded p-3 mb-2 text-wrap">{command}</pre>
            <Button variant="outline-secondary" size="sm" onClick={() => void copy()}>
              {copied ? "Copied" : "Copy the command"}
            </Button>
          </div>
        )}

        {!form || !tokens ? (
          !error && <Spinner animation="border" role="status" size="sm" />
        ) : (
          <>
            <form onSubmit={(e) => void mint(e)}>
              <Form.Group className="mb-3" controlId="mcp-token-label">
                <Form.Label><T text="Label" /></Form.Label>
                <Form.Control
                  type="text"
                  value={form.label}
                  placeholder={t("claude-code on my laptop")}
                  onChange={(e) => setForm({ ...form, label: e.target.value })}
                />
                <Form.Text muted>
                  <T text="What this token is called in the log line every one of its calls writes." />
                </Form.Text>
              </Form.Group>

              <fieldset className="mb-3">
                <legend className="form-label"><T text="This token may" /></legend>
                <Form.Check
                  type="switch"
                  id="mcp-grant-all"
                  className="mb-2"
                  label={t("All grants, including any a later version adds (an unticked box below is still left out)")}
                  checked={form.all}
                  onChange={(e) => setForm({ ...form, all: e.target.checked })}
                />
                {(fields ?? []).map((field) => (
                  <Form.Check
                    key={field.name}
                    type="checkbox"
                    id={`mcp-grant-${field.name}`}
                    label={field.label}
                    checked={form.grants[field.name] === true}
                    onChange={(e) =>
                      setForm({
                        ...form,
                        grants: { ...form.grants, [field.name]: e.target.checked },
                      })
                    }
                  />
                ))}
              </fieldset>

              <Form.Group className="mb-3" controlId="mcp-token-expiry">
                <Form.Label><T text="Expires in (days)" /></Form.Label>
                <Form.Control
                  type="text"
                  value={form.expiresInDays}
                  onChange={(e) => setForm({ ...form, expiresInDays: e.target.value })}
                />
                <Form.Text muted>
                  <T text="Leave empty for a token that never lapses." />
                </Form.Text>
              </Form.Group>

              {problem && form.label !== "" && (
                <div className="text-danger mb-2">{problem}</div>
              )}
              <Button type="submit" variant="outline-primary" disabled={busy || !!problem}>
                {busy ? "Generating…" : "Generate token"}
              </Button>
            </form>

            <h4 className="mt-4 mb-2"><T text="Tokens" /></h4>
            {tokens.length === 0 ? (
              <p className="text-secondary mb-0"><T text="No active tokens." /></p>
            ) : (
              <div className="table-responsive">
                <table className="table table-vcenter">
                  <thead>
                    <tr>
                      <th><T text="Label" /></th>
                      <th><T text="May" /></th>
                      <th><T text="Created" /></th>
                      <th><T text="Last used" /></th>
                      <th><T text="Expires" /></th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    {tokens.map((token) => {
                      const badge = tokenBadge(token);
                      return (
                        <tr key={token.id}>
                          <td>
                            {token.label}{" "}
                            <span className={`badge ${badge.tone} ms-1`}>{badge.label}</span>
                          </td>
                          <td>
                            {holdsAll(token) && (
                              <span className="badge bg-primary-lt me-1">{t("all")}</span>
                            )}
                            {grantedKeys(token).map((key) => (
                              <span className="badge bg-secondary-lt me-1" key={key}>
                                {shortGrantLabel(key)}
                              </span>
                            ))}
                          </td>
                          <td>{when(token.created_at)}</td>
                          <td>{when(token.last_used_at)}</td>
                          <td>{token.expires_at ? when(token.expires_at) : "never"}</td>
                          <td className="text-end">
                            {token.revoked_at ? (
                              <span className="text-secondary">
                                {t("revoked {when}", {
                                  when: when(token.revoked_at),
                                })}
                              </span>
                            ) : (
                              <Button
                                variant="outline-danger"
                                size="sm"
                                onClick={() => void revoke(token)}
                              >
                                <T text="Revoke" />
                              </Button>
                            )}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
            
          </>
        )}
      </div>
    </div>
  );
}
