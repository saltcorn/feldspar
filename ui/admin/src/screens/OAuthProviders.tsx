// The Authentication tab's panel: the external identity providers (OAuth 2.0)
// people sign in with, on the admin UI and in applications.
//
// Like the LLM provider form, **no line here knows a provider's settings**: the
// server declares them (`getOAuthProviderSpec`) and `SettingsFields` renders
// them, client secret included — a password input because the field says
// `secret`. An existing provider's secret arrives as the sentinel, and sending
// it back unchanged keeps what is stored.
//
// What the panel adds is the one address every provider must be told: the
// callback on this host, which is the admin UI's, whichever application a
// sign-in starts from.

import { useEffect, useState, type FormEvent } from "react";
import Alert from "react-bootstrap/Alert";
import Button from "react-bootstrap/Button";
import Modal from "react-bootstrap/Modal";
import Spinner from "react-bootstrap/Spinner";

import { api, errorMessage } from "../api";
import type { ListOAuthProvidersResponse } from "../client";
import { AlertBody } from "../layout";
import { callbackUrl, providerPayload, withPreset } from "../oauthProviders";
import {
  SettingsFields,
  initialValues,
  readConfig,
  type FieldSpec,
} from "../settings";
import { T, useT } from "../i18n";

type Provider = ListOAuthProvidersResponse[number];

/** What the dialog is editing: a new provider, or the one with this name. */
type Editing = { name: string | null; values: Record<string, string> };

export function OAuthProvidersPanel() {
  const { t } = useT();
  const [spec, setSpec] = useState<FieldSpec[] | null>(null);
  const [callbackPath, setCallbackPath] = useState("");
  const [providers, setProviders] = useState<Provider[] | null>(null);
  const [editing, setEditing] = useState<Editing | null>(null);
  const [deleting, setDeleting] = useState<Provider | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [formError, setFormError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    void (async () => {
      try {
        const declared = await api.getOAuthProviderSpec();
        setSpec(declared.fields as FieldSpec[]);
        setCallbackPath(declared.callback_path);
        setProviders(await api.listOAuthProviders());
      } catch (e) {
        setError(errorMessage(e, t("Could not load the identity providers.")));
      }
    })();
  }, []);

  const origin = typeof window === "undefined" ? "" : window.location.origin;

  const open = (provider: Provider | null) => {
    if (!spec) return;
    setFormError(null);
    setEditing({
      name: provider?.name ?? null,
      values: initialValues(spec, provider ? readConfig(provider.config) : {}),
    });
  };

  const change = (name: string, value: string) =>
    setEditing((current) =>
      current && {
        ...current,
        values:
          name === "preset" && current.name === null
            ? withPreset(current.values, value)
            : { ...current.values, [name]: value },
      },
    );

  const save = async (e: FormEvent) => {
    e.preventDefault();
    if (!spec || !editing) return;
    setBusy(true);
    setFormError(null);
    try {
      const body = providerPayload(spec, editing.values);
      if (editing.name === null) await api.createOAuthProvider(body);
      else await api.updateOAuthProvider(editing.name, body);
      setProviders(await api.listOAuthProviders());
      setEditing(null);
    } catch (e) {
      setFormError(errorMessage(e, t("Could not save the identity provider.")));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!deleting) return;
    setBusy(true);
    setError(null);
    try {
      await api.deleteOAuthProvider(deleting.name);
      setProviders(await api.listOAuthProviders());
    } catch (e) {
      setError(errorMessage(e, t("Could not delete the identity provider.")));
    } finally {
      setDeleting(null);
      setBusy(false);
    }
  };

  return (
    <div className="card mb-4">
      <div className="card-header">
        <div>
          <h3 className="card-title">
            <T text="External identity providers" />
          </h3>
          <p className="card-subtitle text-secondary mb-0">
            <T text="OAuth 2.0 providers people can sign in with, on the admin UI and in applications. The lifetime and sharing above apply to these sign-ins too." />
          </p>
        </div>
        <div className="card-actions">
          <Button size="sm" onClick={() => open(null)} disabled={!spec}>
            <T text="New provider" />
          </Button>
        </div>
      </div>
      <div className="card-body">
        {error && (
          <Alert variant="danger" dismissible onClose={() => setError(null)}>
            <AlertBody>{error}</AlertBody>
          </Alert>
        )}
        {callbackPath && (
          <p className="text-secondary">
            <T
              text="Register this redirect URL with each provider: {url}"
              values={{ url: <code>{callbackUrl(origin, callbackPath)}</code> }}
            />
          </p>
        )}
        {!providers ? (
          !error && <Spinner animation="border" role="status" size="sm" />
        ) : providers.length === 0 ? (
          <p className="text-secondary mb-0">
            <T text="No identity providers yet: people sign in with a password only." />
          </p>
        ) : (
          <div className="table-responsive">
            <table className="table table-vcenter card-table">
              <thead>
                <tr>
                  <th><T text="Name" /></th>
                  <th><T text="Button label" /></th>
                  <th><T text="Provider" /></th>
                  <th><T text="Status" /></th>
                  <th className="w-1"></th>
                </tr>
              </thead>
              <tbody>
                {providers.map((provider) => (
                  <tr key={provider.name}>
                    <td><code>{provider.name}</code></td>
                    <td>{provider.label}</td>
                    <td>{provider.preset}</td>
                    <td>
                      {provider.enabled ? (
                        <span className="badge bg-success-lt"><T text="Enabled" /></span>
                      ) : (
                        <span className="badge bg-secondary-lt"><T text="Disabled" /></span>
                      )}
                    </td>
                    <td>
                      <div className="btn-list flex-nowrap">
                        <Button
                          variant="outline-secondary"
                          size="sm"
                          onClick={() => open(provider)}
                        >
                          <T text="Edit" />
                        </Button>
                        <Button
                          variant="outline-danger"
                          size="sm"
                          onClick={() => setDeleting(provider)}
                        >
                          <T text="Delete" />
                        </Button>
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </div>

      <Modal show={editing !== null} onHide={() => setEditing(null)} size="lg" centered>
        <form onSubmit={(e) => void save(e)}>
          <Modal.Header closeButton>
            <Modal.Title>
              {editing?.name === null ? (
                <T text="New identity provider" />
              ) : (
                <T text="Edit identity provider" />
              )}
            </Modal.Title>
          </Modal.Header>
          <Modal.Body>
            {formError && (
              <Alert variant="danger">
                <AlertBody>{formError}</AlertBody>
              </Alert>
            )}
            {spec && editing && (
              <SettingsFields
                spec={spec}
                values={editing.values}
                onChange={change}
                idPrefix="oauth-provider"
              />
            )}
          </Modal.Body>
          <Modal.Footer>
            <Button variant="secondary" onClick={() => setEditing(null)}>
              <T text="Cancel" />
            </Button>
            <Button type="submit" disabled={busy}>
              {busy ? t("Saving…") : t("Save provider")}
            </Button>
          </Modal.Footer>
        </form>
      </Modal>

      <Modal show={deleting !== null} onHide={() => setDeleting(null)} centered>
        <Modal.Header closeButton>
          <Modal.Title>
            <T text="Delete identity provider" />
          </Modal.Title>
        </Modal.Header>
        <Modal.Body>
          <p className="mb-0">
            <T
              text="Nobody will be able to sign in with {name} any more. Accounts made through it stay, and can still sign in with a password."
              values={{ name: <strong>{deleting?.label}</strong> }}
            />
          </p>
        </Modal.Body>
        <Modal.Footer>
          <Button variant="secondary" onClick={() => setDeleting(null)}>
            <T text="Cancel" />
          </Button>
          <Button variant="danger" onClick={() => void remove()} disabled={busy}>
            <T text="Delete" />
          </Button>
        </Modal.Footer>
      </Modal>
    </div>
  );
}
