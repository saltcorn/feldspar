// The "Identity provider" tab's clients, grants and keys (TODO IP1.8).
//
// The switch that turns the provider on is part of the application form and
// saved with it; this panel comes after the form (as the translations editor
// does), because each of its actions is a request of its own: registering a
// client, editing or deleting one, a new secret, withdrawing a grant, rotating
// the signing keys. A client secret is shown once, when it is made.

import { useEffect, useState } from "react";
import Alert from "react-bootstrap/Alert";
import Badge from "react-bootstrap/Badge";
import Button from "react-bootstrap/Button";
import Card from "react-bootstrap/Card";
import Form from "react-bootstrap/Form";
import Modal from "react-bootstrap/Modal";
import Table from "react-bootstrap/Table";

import { api, errorMessage } from "../api";
import type { GetIdentityProviderResponse } from "../client";
import { appUrl } from "../adminAddress";
import {
  OIDC_SCOPES,
  blankClientForm,
  clientFormOf,
  clientName,
  clientRequest,
  providerAddresses,
  type ClientForm,
  type OAuthClientItem,
} from "../identityProvider";
import { T, useT } from "../i18n";

/** What the form is open on: a new client, or one being edited. */
type Editing = { clientId: string | null; form: ClientForm };

export function IdentityProviderPanel({
  appId,
  subdomain,
  enabled,
}: {
  appId: string;
  subdomain: string;
  /** The switch as the form has it now, saved or about to be. */
  enabled: boolean;
}) {
  const { t } = useT();
  const [data, setData] = useState<GetIdentityProviderResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Editing | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  /** A secret just made, and the client it is for: shown this once. */
  const [secret, setSecret] = useState<{ client: string; secret: string } | null>(null);

  const reload = async () => {
    try {
      setData(await api.getIdentityProvider(appId));
      setError(null);
    } catch (err) {
      setError(errorMessage(err, "Could not load the identity provider."));
    }
  };
  useEffect(() => {
    void reload();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [appId]);

  const { issuer, discovery } = providerAddresses(appUrl(subdomain));
  const addedFields = (data?.claim_sources ?? []).filter((f) => f !== "role");

  const act = async (what: () => Promise<void>, failure: string) => {
    setBusy(true);
    try {
      await what();
      await reload();
    } catch (err) {
      setError(errorMessage(err, failure));
    } finally {
      setBusy(false);
    }
  };

  const save = async () => {
    if (!editing) return;
    const request = clientRequest(editing.form);
    if (!request.ok) {
      setFormError(
        t("Line {line} of the claims is not “claim = field”.", { line: request.claimLine }),
      );
      return;
    }
    setBusy(true);
    try {
      if (editing.clientId === null) {
        const made = await api.createOAuthClient(appId, request.body);
        if (made.secret) setSecret({ client: made.client.name, secret: made.secret });
      } else {
        const { confidential: _confidential, ...update } = request.body;
        await api.updateOAuthClient(appId, editing.clientId, update);
      }
      setEditing(null);
      setFormError(null);
      await reload();
    } catch (err) {
      setFormError(errorMessage(err, "Could not save the client."));
    } finally {
      setBusy(false);
    }
  };

  const regenerate = (client: OAuthClientItem) =>
    act(async () => {
      const fresh = await api.regenerateOAuthClientSecret(appId, client.client_id);
      setSecret({ client: client.name, secret: fresh.secret });
    }, "Could not make a new secret.");

  const remove = (client: OAuthClientItem) => {
    if (!window.confirm(t("Delete {name}? Everyone signed in to it through this application is signed out of it.", { name: client.name }))) {
      return;
    }
    void act(async () => {
      await api.deleteOAuthClient(appId, client.client_id);
    }, "Could not delete the client.");
  };

  const setForm = (patch: Partial<ClientForm>) =>
    setEditing((e) => (e ? { ...e, form: { ...e.form, ...patch } } : e));

  return (
    <Card className="mb-3">
      <Card.Header className="d-flex justify-content-between align-items-center">
        <span>
          <T text="Clients" />
        </span>
        <Button
          size="sm"
          variant="outline-primary"
          disabled={busy}
          onClick={() => {
            setFormError(null);
            setEditing({ clientId: null, form: blankClientForm() });
          }}
        >
          <T text="Register a client" />
        </Button>
      </Card.Header>
      <Card.Body>
        {error && <Alert variant="danger">{error}</Alert>}
        {!enabled && (
          <Alert variant="secondary">
            <T text="The provider is off: its addresses answer 404 and nobody can sign in through it until it is switched on." />
          </Alert>
        )}
        <dl className="row small mb-3">
          <dt className="col-sm-3">
            <T text="Issuer" />
          </dt>
          <dd className="col-sm-9">
            <code>{issuer}</code>
          </dd>
          <dt className="col-sm-3">
            <T text="Discovery URL" />
          </dt>
          <dd className="col-sm-9">
            <code>{discovery}</code>
          </dd>
        </dl>

        {secret && (
          <Alert variant="warning" dismissible onClose={() => setSecret(null)}>
            <strong>
              <T text="The client secret for {name} is shown only now." values={{ name: secret.client }} />
            </strong>{" "}
            <T text="Copy it into the service's settings. It is stored only as a hash; if it is lost, make a new one." />
            <div className="mt-2">
              <code className="user-select-all">{secret.secret}</code>
            </div>
          </Alert>
        )}

        {data && data.clients.length === 0 && (
          <div className="text-muted">
            <T text="No clients yet. Register the service that will sign people in with this application's users." />
          </div>
        )}
        {data && data.clients.length > 0 && (
          <Table size="sm" responsive>
            <thead>
              <tr>
                <th><T text="Name" /></th>
                <th><T text="Client ID" /></th>
                <th><T text="Redirect URIs" /></th>
                <th />
              </tr>
            </thead>
            <tbody>
              {data.clients.map((client) => (
                <tr key={client.client_id}>
                  <td>
                    {client.name}{" "}
                    {!client.confidential && (
                      <Badge bg="secondary">{t("public")}</Badge>
                    )}
                  </td>
                  <td>
                    <code className="user-select-all">{client.client_id}</code>
                  </td>
                  <td className="small">
                    {client.redirect_uris.map((uri) => (
                      <div key={uri}>{uri}</div>
                    ))}
                  </td>
                  <td className="text-end text-nowrap">
                    <Button
                      size="sm"
                      variant="link"
                      onClick={() => {
                        setFormError(null);
                        setEditing({ clientId: client.client_id, form: clientFormOf(client) });
                      }}
                    >
                      <T text="Edit" />
                    </Button>
                    {client.confidential && (
                      <Button size="sm" variant="link" disabled={busy} onClick={() => void regenerate(client)}>
                        <T text="New secret" />
                      </Button>
                    )}
                    <Button size="sm" variant="link" className="text-danger" disabled={busy} onClick={() => remove(client)}>
                      <T text="Delete" />
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}

        <h6 className="mt-4">
          <T text="Signed-in users" />
        </h6>
        {data && data.grants.length === 0 && (
          <div className="text-muted small">
            <T text="Nobody has signed in to a client yet." />
          </div>
        )}
        {data && data.grants.length > 0 && (
          <Table size="sm" responsive>
            <thead>
              <tr>
                <th><T text="User" /></th>
                <th><T text="Client" /></th>
                <th><T text="Scopes" /></th>
                <th />
              </tr>
            </thead>
            <tbody>
              {data.grants.map((grant) => (
                <tr key={grant.id}>
                  <td>{grant.email ?? grant.user_id}</td>
                  <td>{clientName(data.clients, grant.client_id)}</td>
                  <td className="small">{grant.scopes.join(" ")}</td>
                  <td className="text-end">
                    <Button
                      size="sm"
                      variant="link"
                      className="text-danger"
                      disabled={busy}
                      onClick={() =>
                        void act(async () => {
                          await api.revokeOAuthGrant(appId, grant.id);
                        }, "Could not revoke the grant.")
                      }
                    >
                      <T text="Revoke" />
                    </Button>
                  </td>
                </tr>
              ))}
            </tbody>
          </Table>
        )}

        <h6 className="mt-4">
          <T text="Signing keys" />
        </h6>
        <Form.Text muted className="d-block mb-2">
          <T text="One key set signs for every application on this server. Rotating it makes new keys; the old ones stay published for a day, so tokens they signed still check." />
        </Form.Text>
        {data && (
          <ul className="small">
            {data.signing_keys.map((key) => (
              <li key={key.kid}>
                <code>{key.alg}</code> {key.kid.slice(0, 12)}…{" "}
                {key.retired_at ? (
                  <span className="text-muted">{t("retired")}</span>
                ) : (
                  <span className="text-success">{t("signing")}</span>
                )}
              </li>
            ))}
            {data.signing_keys.length === 0 && (
              <li className="text-muted">
                <T text="None yet: they are made the first time one is needed." />
              </li>
            )}
          </ul>
        )}
        <Button
          size="sm"
          variant="outline-secondary"
          disabled={busy}
          onClick={() => {
            if (window.confirm(t("Rotate the signing keys for every application on this server?"))) {
              void act(async () => {
                await api.rotateOidcSigningKeys();
              }, "Could not rotate the signing keys.");
            }
          }}
        >
          <T text="Rotate keys" />
        </Button>
      </Card.Body>

      <Modal show={editing !== null} onHide={() => setEditing(null)} size="lg">
        <Modal.Header closeButton>
          <Modal.Title>
            {editing?.clientId === null ? t("Register a client") : t("Edit client")}
          </Modal.Title>
        </Modal.Header>
        {editing && (
          <Modal.Body>
            {formError && <Alert variant="danger">{formError}</Alert>}
            <Form.Group className="mb-3" controlId="oauthClientName">
              <Form.Label><T text="Name" /></Form.Label>
              <Form.Control
                value={editing.form.name}
                onChange={(e) => setForm({ name: e.target.value })}
              />
              <Form.Text muted>
                <T text="The consent page shows it to the person signing in." />
              </Form.Text>
            </Form.Group>
            <Form.Check
              className="mb-3"
              type="switch"
              id="oauthClientConfidential"
              disabled={editing.clientId !== null}
              checked={editing.form.confidential}
              onChange={(e) => setForm({ confidential: e.target.checked })}
              label={t("Confidential: a server-side service that keeps a client secret")}
            />
            <Form.Group className="mb-3" controlId="oauthClientRedirects">
              <Form.Label><T text="Redirect URIs" /></Form.Label>
              <Form.Control
                as="textarea"
                rows={2}
                className="font-monospace"
                value={editing.form.redirectUris}
                placeholder="https://grafana.example.com/login/generic_oauth"
                onChange={(e) => setForm({ redirectUris: e.target.value })}
              />
              <Form.Text muted>
                <T text="One per line, compared exactly. https, or http only to localhost." />
              </Form.Text>
            </Form.Group>
            <Form.Group className="mb-3">
              <Form.Label><T text="Scopes it may ask for" /></Form.Label>
              <div>
                {OIDC_SCOPES.map((scope) => (
                  <Form.Check
                    inline
                    key={scope}
                    id={`oauthScope-${scope}`}
                    label={scope}
                    checked={editing.form.scopes.includes(scope)}
                    onChange={(e) =>
                      setForm({
                        scopes: e.target.checked
                          ? [...editing.form.scopes, scope]
                          : editing.form.scopes.filter((s) => s !== scope),
                      })
                    }
                  />
                ))}
              </div>
              <Form.Text muted>
                <T text="None ticked: all of them." />
              </Form.Text>
            </Form.Group>
            <Form.Check
              className="mb-3"
              type="switch"
              id="oauthClientSkipConsent"
              checked={editing.form.skipConsent}
              onChange={(e) => setForm({ skipConsent: e.target.checked })}
              label={t("First-party: don't ask its users to consent")}
            />
            <Form.Group className="mb-3" controlId="oauthClientClaims">
              <Form.Label><T text="Further claims" /></Form.Label>
              <Form.Control
                as="textarea"
                rows={2}
                className="font-monospace"
                value={editing.form.claims}
                placeholder="groups = role"
                onChange={(e) => setForm({ claims: e.target.value })}
              />
              <Form.Text muted>
                {addedFields.length > 0 ? (
                  <T
                    text="One {mapping} per line, given in the ID token and at userinfo. A field is {role} (the role's name) or one of the users table's own: {fields}."
                    values={{
                      mapping: <code>claim = field</code>,
                      role: <code>role</code>,
                      fields: addedFields.join(", "),
                    }}
                  />
                ) : (
                  <T
                    text="One {mapping} per line, given in the ID token and at userinfo. A field is {role} (the role's name), or a field added to the users table; none has been yet."
                    values={{ mapping: <code>claim = field</code>, role: <code>role</code> }}
                  />
                )}
              </Form.Text>
            </Form.Group>
            <Form.Group className="mb-3" controlId="oauthClientLogout">
              <Form.Label><T text="Post-logout redirect URIs" /></Form.Label>
              <Form.Control
                as="textarea"
                rows={2}
                className="font-monospace"
                value={editing.form.logoutUris}
                onChange={(e) => setForm({ logoutUris: e.target.value })}
              />
              <Form.Text muted>
                <T text="Where signing out from the service may return to, one per line. Empty: nowhere." />
              </Form.Text>
            </Form.Group>
          </Modal.Body>
        )}
        <Modal.Footer>
          <Button variant="secondary" onClick={() => setEditing(null)}>
            <T text="Cancel" />
          </Button>
          <Button disabled={busy} onClick={() => void save()}>
            {editing?.clientId === null ? t("Register") : t("Save")}
          </Button>
        </Modal.Footer>
      </Modal>
    </Card>
  );
}
